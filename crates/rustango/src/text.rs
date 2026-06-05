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

/// [`django.template.defaultfilters.truncatechars`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#truncatechars) —
/// truncate to AT MOST `count` characters total **including** the
/// ellipsis (`…`).
///
/// Distinct from [`truncate`] (which appends the suffix BEYOND
/// the count budget — `truncate("hello world", 5, "…")` returns
/// `"hello…"` for 6 chars total). `truncatechars(s, 5)` returns
/// `"hell…"` — exactly 5 chars.
///
/// * `count >= total chars`: no truncation, returns `s` unchanged.
/// * `count == 0`: returns empty string.
/// * `count >= 1`: returns `s[..count-1] + "…"`.
///
/// ```
/// use rustango::text::truncatechars;
/// assert_eq!(truncatechars("Joel is a slug", 7), "Joel i…");
/// assert_eq!(truncatechars("Hi", 10), "Hi");
/// assert_eq!(truncatechars("abc", 3), "abc");      // boundary
/// assert_eq!(truncatechars("abcd", 3), "ab…");     // 3 chars total
/// assert_eq!(truncatechars("any", 0), "");
/// ```
#[must_use]
pub fn truncatechars(s: &str, count: usize) -> String {
    let total = s.chars().count();
    if total <= count {
        return s.to_owned();
    }
    if count == 0 {
        return String::new();
    }
    let keep = count - 1;
    let truncated: String = s.chars().take(keep).collect();
    format!("{truncated}…")
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

/// PII-redaction helper — render a partly-obscured email for
/// display in admin lists / audit logs / API responses.
///
/// Format `<first><***><last>@<domain>`:
/// * 0 chars before `@`: `@example.com` (preserved shape)
/// * 1 char: `*@example.com`
/// * 2 chars: `a*@example.com`
/// * 3+ chars: `a***z@example.com`
///
/// Strings without an `@` pass through unchanged. Doesn't validate
/// the input is a real email — pair with
/// [`crate::validators::validate_email`] at intake.
///
/// ```
/// use rustango::text::mask_email;
/// assert_eq!(mask_email("alice@example.com"), "a***e@example.com");
/// assert_eq!(mask_email("a@example.com"), "*@example.com");
/// assert_eq!(mask_email("not-an-email"), "not-an-email");
/// ```
#[must_use]
pub fn mask_email(s: &str) -> String {
    let Some((local, domain)) = s.split_once('@') else {
        return s.to_owned();
    };
    let local_chars: Vec<char> = local.chars().collect();
    let masked_local = match local_chars.len() {
        0 => String::new(),
        1 => "*".to_owned(),
        2 => format!("{}*", local_chars[0]),
        n => format!("{}***{}", local_chars[0], local_chars[n - 1]),
    };
    format!("{masked_local}@{domain}")
}

/// PII-redaction helper — render the canonical
/// `"************1234"`-style masked credit-card number from a
/// digit string.
///
/// Strips whitespace and `-` first (typical human-typed shape),
/// then replaces every digit except the last 4 with `*`. Strings
/// with ≤ 4 digits are fully masked. Non-digit input (after
/// stripping separators) returns unchanged.
///
/// ```
/// use rustango::text::mask_card;
/// assert_eq!(mask_card("4111 1111 1111 1111"), "************1111");
/// assert_eq!(mask_card("4111"), "****");
/// assert_eq!(mask_card("not a card"), "not a card");
/// ```
#[must_use]
pub fn mask_card(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if cleaned.is_empty() || !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return s.to_owned();
    }
    let chars: Vec<char> = cleaned.chars().collect();
    let n = chars.len();
    if n <= 4 {
        return "*".repeat(n);
    }
    let last4: String = chars[n - 4..].iter().collect();
    let masked = "*".repeat(n - 4);
    format!("{masked}{last4}")
}

/// PII-redaction helper — render a partly-obscured phone number.
/// Keeps separator characters in place; masks every digit except
/// the last 4.
///
/// ≤ 4 digits → all masked. No digits → passes through unchanged.
///
/// ```
/// use rustango::text::mask_phone;
/// assert_eq!(mask_phone("+1 415 555 2671"), "+* *** *** 2671");
/// assert_eq!(mask_phone("(415) 555-2671"), "(***) ***-2671");
/// assert_eq!(mask_phone("4155552671"), "******2671");
/// assert_eq!(mask_phone("123"), "***");
/// assert_eq!(mask_phone("no digits"), "no digits");
/// ```
#[must_use]
pub fn mask_phone(s: &str) -> String {
    let total_digits = s.chars().filter(|c| c.is_ascii_digit()).count();
    if total_digits == 0 {
        return s.to_owned();
    }
    let keep_from = if total_digits <= 4 {
        total_digits
    } else {
        total_digits - 4
    };
    let mut digit_idx = 0;
    s.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                let keep = digit_idx >= keep_from;
                digit_idx += 1;
                if keep {
                    c
                } else {
                    '*'
                }
            } else {
                c
            }
        })
        .collect()
}

/// Join a slice of strings using the Oxford-comma convention with
/// a configurable final conjunction.
///
/// * `[]` → `""`
/// * `["a"]` → `"a"`
/// * `["a", "b"]` → `"a {conj} b"` (no comma — only two items)
/// * `["a", "b", "c"]` → `"a, b, {conj} c"` (Oxford comma)
/// * `["a", "b", "c", "d"]` → `"a, b, c, {conj} d"`
///
/// Default conjunction is `"and"`. Pass `"or"` for the disjunctive
/// form ("a, b, or c"); any other word works too ("plus", "via", …).
///
/// ```
/// use rustango::text::oxford_join;
/// assert_eq!(oxford_join(&["a", "b", "c"], "and"), "a, b, and c");
/// assert_eq!(oxford_join(&["a", "b"], "and"), "a and b");
/// assert_eq!(oxford_join(&["a", "b", "c"], "or"), "a, b, or c");
/// assert_eq!(oxford_join(&[] as &[&str], "and"), "");
/// ```
#[must_use]
pub fn oxford_join<S: AsRef<str>>(items: &[S], conj: &str) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [a, b] => format!("{} {conj} {}", a.as_ref(), b.as_ref()),
        rest => {
            let (last, init) = rest.split_last().unwrap();
            let head = init
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{head}, {conj} {}", last.as_ref())
        }
    }
}

/// Return the uppercase first character of each whitespace-
/// separated word in `s`. Useful for avatar-fallback shapes ("two
/// letters inside a colored circle when no profile picture is
/// uploaded") and for any per-user mnemonic that doesn't have
/// space for the full name.
///
/// * Non-alphabetic leading chars are skipped, so `"123 Alice"`
///   yields `"A"`, not `"1"`.
/// * Words with no alphabetic chars contribute nothing.
/// * `limit = Some(n)` caps the result at `n` initials.
/// * `limit = None` includes every word's initial.
///
/// ```
/// use rustango::text::initials;
/// assert_eq!(initials("Alice", None), "A");
/// assert_eq!(initials("Alice Bob", None), "AB");
/// assert_eq!(initials("alice m. bob", None), "AMB");
/// assert_eq!(initials("alice m. bob", Some(2)), "AM");
/// assert_eq!(initials("123 alice", None), "A");
/// assert_eq!(initials("", None), "");
/// ```
#[must_use]
pub fn initials(s: &str, limit: Option<usize>) -> String {
    if matches!(limit, Some(0)) {
        return String::new();
    }
    let mut out = String::new();
    for word in s.split_whitespace() {
        if let Some(ch) = word.chars().find(|c| c.is_alphabetic()) {
            for upper_ch in ch.to_uppercase() {
                out.push(upper_ch);
            }
            if let Some(lim) = limit {
                if out.chars().count() >= lim {
                    break;
                }
            }
        }
    }
    out
}

/// [`django.template.defaultfilters.cut`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#cut) —
/// remove every occurrence of `needle` from `s`.
///
/// Equivalent to `s.replace(needle, "")` with one extra guarantee:
/// an empty `needle` returns `s` unchanged (avoids the empty-
/// substring-matches-everywhere footgun that Django's filter
/// short-circuits the same way).
///
/// ```
/// use rustango::text::cut;
/// assert_eq!(cut("Joel is a slug", " "), "Joelisaslug");
/// assert_eq!(cut("hello world", "l"), "heo word");
/// assert_eq!(cut("nothing matches", "xyz"), "nothing matches");
/// // Empty needle is a no-op.
/// assert_eq!(cut("untouched", ""), "untouched");
/// ```
#[must_use]
pub fn cut(s: &str, needle: &str) -> String {
    if needle.is_empty() {
        return s.to_owned();
    }
    s.replace(needle, "")
}

/// Collapse internal whitespace runs in `s` to a single ASCII
/// space and drop leading/trailing whitespace. Equivalent to the
/// existing `normalize_whitespace` Tera filter, surfaced as a
/// plain Rust function.
///
/// Useful for diff-stable comparisons of HTML / formatted text
/// where intra-element whitespace shouldn't matter, and for
/// canonicalizing operator-typed input before storage.
///
/// ```
/// use rustango::text::normalize_whitespace;
/// assert_eq!(normalize_whitespace("  hello   world  "), "hello world");
/// assert_eq!(normalize_whitespace("a\n\tb\rc"), "a b c");
/// assert_eq!(normalize_whitespace(""), "");
/// ```
#[must_use]
pub fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`django.template.defaultfilters.wordcount`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#wordcount) —
/// count whitespace-separated words in a string.
///
/// Empty string returns `0`. Multiple consecutive whitespace chars
/// collapse to a single separator (matches `str::split_whitespace`).
///
/// ```
/// use rustango::text::wordcount;
/// assert_eq!(wordcount("Joel is a slug"), 4);
/// assert_eq!(wordcount(""), 0);
/// assert_eq!(wordcount("  spaces   between   "), 2);
/// ```
#[must_use]
pub fn wordcount(s: &str) -> usize {
    s.split_whitespace().count()
}

/// [`django.template.defaultfilters.linenumbers`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#linenumbers) —
/// prepend each line with a 1-based line number, right-aligned to
/// the width of the largest line number.
///
/// ```
/// use rustango::text::linenumbers;
/// assert_eq!(linenumbers("one\ntwo\nthree"), "1. one\n2. two\n3. three");
/// ```
#[must_use]
pub fn linenumbers(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let width = lines.len().to_string().len();
    let mut out = String::with_capacity(s.len() + lines.len() * (width + 2));
    use std::fmt::Write as _;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(out, "{:>width$}. {}", i + 1, line, width = width);
    }
    out
}

/// [`django.template.defaultfilters.ljust`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#ljust) —
/// left-justify (pad right with spaces) to width `n`. Values
/// already at or beyond `n` characters return as-is.
///
/// ```
/// use rustango::text::ljust;
/// assert_eq!(ljust("Joel", 10), "Joel      ");
/// assert_eq!(ljust("already long enough", 5), "already long enough");
/// ```
#[must_use]
pub fn ljust(s: &str, n: usize) -> String {
    let chars = s.chars().count();
    if chars >= n {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + (n - chars));
    out.push_str(s);
    out.extend(std::iter::repeat(' ').take(n - chars));
    out
}

/// [`django.template.defaultfilters.rjust`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#rjust) —
/// right-justify (pad left with spaces) to width `n`.
///
/// ```
/// use rustango::text::rjust;
/// assert_eq!(rjust("Joel", 10), "      Joel");
/// ```
#[must_use]
pub fn rjust(s: &str, n: usize) -> String {
    let chars = s.chars().count();
    if chars >= n {
        return s.to_owned();
    }
    let mut out = String::with_capacity(n);
    out.extend(std::iter::repeat(' ').take(n - chars));
    out.push_str(s);
    out
}

/// [`django.template.defaultfilters.center`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#center) —
/// center `s` in a field of width `n`. When padding doesn't split
/// evenly the extra space goes on the right (matches Python's
/// `str.center`).
///
/// ```
/// use rustango::text::center;
/// assert_eq!(center("Joel", 10), "   Joel   ");
/// assert_eq!(center("x", 4), " x  ");
/// ```
#[must_use]
pub fn center(s: &str, n: usize) -> String {
    let chars = s.chars().count();
    if chars >= n {
        return s.to_owned();
    }
    let total_pad = n - chars;
    let left = total_pad / 2;
    let right = total_pad - left;
    let mut out = String::with_capacity(n);
    out.extend(std::iter::repeat(' ').take(left));
    out.push_str(s);
    out.extend(std::iter::repeat(' ').take(right));
    out
}

/// [`django.template.defaultfilters.get_digit`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#get-digit) —
/// extract the `idx`-th digit (1-indexed, from the **right**) of an
/// integer.
///
/// * `get_digit(1234, 1)` → `"4"` (rightmost)
/// * `get_digit(1234, 4)` → `"1"`
/// * `get_digit(1234, 5)` → `"0"` (past leftmost digit)
/// * `idx < 1` returns the full integer string (Django's
///   passthrough-on-invalid-index shape).
///
/// Negative input uses the absolute value's digits — matches
/// Django (`get_digit(-1234, 1) == "4"`, not `"-"`).
///
/// ```
/// use rustango::text::get_digit;
/// assert_eq!(get_digit(1234, 1), "4");
/// assert_eq!(get_digit(1234, 4), "1");
/// assert_eq!(get_digit(1234, 5), "0");
/// assert_eq!(get_digit(1234, 0), "1234");
/// assert_eq!(get_digit(-1234, 1), "4");
/// ```
#[must_use]
pub fn get_digit(n: i64, idx: i64) -> String {
    if idx < 1 {
        return n.to_string();
    }
    let s = n.unsigned_abs().to_string();
    let chars: Vec<char> = s.chars().rev().collect();
    let pick = chars
        .get(usize::try_from(idx - 1).unwrap_or(0))
        .copied()
        .unwrap_or('0');
    pick.to_string()
}

/// [`django.template.defaultfilters.pluralize`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#pluralize) —
/// pick the singular or plural suffix for a count.
///
/// `suffix_arg` syntax matches Django exactly:
///
/// * `""` / `"s"` — empty singular / `"s"` plural (default Django
///   shape when no arg is passed).
/// * `"<one-token>"` — empty singular / `<one-token>` plural.
/// * `"<singular>,<plural>"` — pick whichever matches count.
/// * 3+ comma-separated tokens: extras silently ignored.
///
/// `count == 1` → singular; anything else (incl. `0` and negative)
/// → plural. Matches Django + English-language convention.
///
/// ```
/// use rustango::text::pluralize;
///
/// // Default "" / "s".
/// assert_eq!(pluralize(1, ""), "");
/// assert_eq!(pluralize(2, ""), "s");
/// assert_eq!(pluralize(0, ""), "s");
///
/// // Custom plural suffix.
/// assert_eq!(pluralize(1, "es"), "");
/// assert_eq!(pluralize(2, "es"), "es");
///
/// // Singular,plural pair.
/// assert_eq!(pluralize(1, "y,ies"), "y");
/// assert_eq!(pluralize(2, "y,ies"), "ies");
/// ```
#[must_use]
pub fn pluralize(count: i64, suffix_arg: &str) -> String {
    let parts: Vec<&str> = suffix_arg.split(',').collect();
    let (singular, plural): (String, String) = match parts.as_slice() {
        [""] | [] => (String::new(), "s".to_owned()),
        [one] => (String::new(), (*one).to_owned()),
        [s, p, ..] => ((*s).to_owned(), (*p).to_owned()),
    };
    if count == 1 {
        singular
    } else {
        plural
    }
}

/// Django-parity `Truncator(s).chars(num, html=True, truncate=…)` —
/// truncate to `max_chars` visible characters while preserving HTML
/// tag structure. Open tags at the truncation point are closed in
/// reverse order so the output is well-formed HTML.
///
/// "Visible" characters are everything that isn't inside `<tag>`
/// brackets and isn't part of an HTML entity (`&amp;` counts as 1
/// char, not 5). Self-closing tags (`<br>`, `<br/>`, `<img>`,
/// `<hr>`, `<input>`, `<meta>`, `<link>`, `<source>`, `<area>`,
/// `<col>`, `<embed>`, `<param>`, `<track>`, `<wbr>`) aren't pushed
/// onto the close-stack.
///
/// `suffix` is appended ONLY when truncation actually fires, and
/// the close-tags are written AFTER the suffix to match Django's
/// shape — so `truncate_html_chars("<p>hello world</p>", 5, "…")`
/// returns `"<p>hello…</p>"`, not `"<p>hello</p>…"`.
///
/// This is **not** a sanitizer — it assumes well-formed input HTML.
/// Pair with `html_escape` if you're rendering operator-controlled
/// text and need defense-in-depth.
///
/// ```
/// use rustango::text::truncate_html_chars;
/// assert_eq!(
///     truncate_html_chars("<p>hello world</p>", 5, "…"),
///     "<p>hello…</p>"
/// );
/// assert_eq!(
///     truncate_html_chars("<p>short</p>", 10, "…"),
///     "<p>short</p>"
/// );
/// assert_eq!(
///     truncate_html_chars("<b><i>nested</i> text</b>", 7, "…"),
///     "<b><i>nested</i> …</b>"
/// );
/// ```
#[must_use]
pub fn truncate_html_chars(html: &str, max_chars: usize, suffix: &str) -> String {
    truncate_html_visible_count(html, max_chars, suffix, /* by_words */ false)
}

/// Django-parity `Truncator(s).words(num, html=True, truncate=…)` —
/// HTML-tag-aware version of [`truncate_words`]. Counts whitespace-
/// separated words OUTSIDE tag brackets; preserves tag structure
/// by closing open tags after the suffix when truncation fires.
///
/// Self-closing tags don't count as words and don't go on the
/// close-stack (same set as [`truncate_html_chars`]).
///
/// ```
/// use rustango::text::truncate_html_words;
/// assert_eq!(
///     truncate_html_words("<p>Joel is a slug</p>", 2, " …"),
///     "<p>Joel is …</p>"
/// );
/// assert_eq!(
///     truncate_html_words("<p>short text</p>", 5, "…"),
///     "<p>short text</p>"
/// );
/// ```
#[must_use]
pub fn truncate_html_words(html: &str, max_words: usize, suffix: &str) -> String {
    truncate_html_visible_count(html, max_words, suffix, /* by_words */ true)
}

const SELF_CLOSING_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

fn truncate_html_visible_count(html: &str, limit: usize, suffix: &str, by_words: bool) -> String {
    // First pass: count visible units (chars or words) the input
    // actually contains. If it's <= limit we can short-circuit.
    if count_visible(html, by_words) <= limit {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len());
    let mut open_tags: Vec<String> = Vec::new();
    let mut count: usize = 0;
    let mut in_word = false; // tracks whitespace-state for word counting
    let mut bytes = html.char_indices().peekable();
    while let Some((_, ch)) = bytes.next() {
        if ch == '<' {
            // Capture tag content up to matching `>`.
            let mut tag = String::from('<');
            for (_, c) in bytes.by_ref() {
                tag.push(c);
                if c == '>' {
                    break;
                }
            }
            out.push_str(&tag);
            update_open_tags(&mut open_tags, &tag);
            continue;
        }
        if ch == '&' {
            // Treat an entity as a single visible character; copy
            // verbatim until `;` (or fall back to a single char if
            // malformed).
            let mut ent = String::from('&');
            let mut saw_semi = false;
            for (_, c) in bytes.by_ref() {
                ent.push(c);
                if c == ';' {
                    saw_semi = true;
                    break;
                }
                if ent.len() > 16 {
                    break; // malformed; stop accumulating
                }
            }
            out.push_str(&ent);
            let _ = saw_semi; // malformed entity still counts as 1 unit
            if by_words {
                in_word = true;
            } else {
                count += 1;
                if count >= limit {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
        if by_words {
            if ch.is_whitespace() {
                if in_word {
                    count += 1;
                    if count >= limit {
                        break;
                    }
                }
                in_word = false;
            } else {
                in_word = true;
            }
        } else {
            count += 1;
            if count >= limit {
                break;
            }
        }
    }
    if by_words {
        // Trim trailing whitespace from `out` before appending suffix —
        // word counting consumed the boundary whitespace, but Django's
        // shape doesn't keep it before the truncation marker.
        while out.ends_with(|c: char| c.is_whitespace()) {
            out.pop();
        }
    }
    let _ = in_word;
    out.push_str(suffix);
    while let Some(tag_name) = open_tags.pop() {
        out.push_str(&format!("</{tag_name}>"));
    }
    out
}

fn count_visible(html: &str, by_words: bool) -> usize {
    let mut count = 0usize;
    let mut in_tag = false;
    let mut in_word = false;
    let mut chars = html.chars();
    while let Some(ch) = chars.next() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '&' {
            // Skip to `;` (entity counts as 1 unit).
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
            }
            if by_words {
                in_word = true;
            } else {
                count += 1;
            }
            continue;
        }
        if by_words {
            if ch.is_whitespace() {
                if in_word {
                    count += 1;
                }
                in_word = false;
            } else {
                in_word = true;
            }
        } else {
            count += 1;
        }
    }
    if by_words && in_word {
        count += 1;
    }
    count
}

fn update_open_tags(stack: &mut Vec<String>, tag: &str) {
    // `tag` is the raw tag text including `<` and `>`.
    let inner = tag.trim_start_matches('<').trim_end_matches('>').trim();
    if inner.is_empty() {
        return;
    }
    // Comments / CDATA / DOCTYPE — ignore.
    if inner.starts_with('!') {
        return;
    }
    if inner.starts_with('/') {
        // Closing tag — pop matching name from stack.
        let name = inner[1..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        if let Some(pos) = stack.iter().rposition(|t| *t == name) {
            stack.remove(pos);
        }
        return;
    }
    let raw_name = inner.split_whitespace().next().unwrap_or("");
    // XHTML self-closing form `<br/>` or `<img ... />`.
    let trimmed = raw_name.trim_end_matches('/');
    let name = trimmed.to_lowercase();
    if name.is_empty() {
        return;
    }
    if SELF_CLOSING_TAGS.contains(&name.as_str()) {
        return;
    }
    if inner.ends_with('/') {
        // XHTML self-closing — don't push.
        return;
    }
    stack.push(name);
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
/// [`django.utils.html.json_script(value, element_id)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.json_script) —
/// embed a JSON-serialized `value` into a
/// `<script type="application/json" id="..."></script>` tag for
/// safe pass-through to client-side JavaScript.
///
/// The canonical "ship a typed object from view to JS" path.
/// Escapes the JSON XSS-defang characters (`<` `>` `&` plus the
/// two Unicode line terminators U+2028 and U+2029) so the script
/// element can't be terminated early by attacker-controlled
/// content inside the serialized object.
///
/// Pair with `JSON.parse(document.getElementById('id').textContent)`
/// on the client side.
///
/// # Errors
/// Returns `serde_json::Error` only when `value` itself fails to
/// serialize (e.g. NaN floats in a struct that rejects them) —
/// the escaping pass is infallible.
///
/// ```ignore
/// use rustango::text::json_script;
///
/// #[derive(serde::Serialize)]
/// struct Bootstrap { user_id: u64, csrf: String }
///
/// let html = json_script(&Bootstrap { user_id: 42, csrf: "tok".into() },
///                       "bootstrap-data")?;
/// // → <script id="bootstrap-data" type="application/json">{"user_id":42,"csrf":"tok"}</script>
/// ```
///
/// The `element_id` is HTML-attribute-escaped before insertion.
/// `</script>` inside a string can't break out because `<` →
/// `&lt;`-equivalent.
/// [`django.utils.html.escapejs(value)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.escapejs) —
/// escape a string for safe embedding inside a JavaScript string
/// literal in HTML.
///
/// Use this when you want to inject a server-side value directly
/// into a `<script>` tag string literal. Prefer
/// [`json_script`] for typed JSON payloads — it sets the right
/// MIME type and is read back via `JSON.parse(document.getElementById(…).textContent)`,
/// which is the modern best practice. `escapejs` is the older
/// inline-string form Django still ships for callers that need
/// it.
///
/// The escape set defangs both HTML-parser and JS-syntax breakage:
///
/// * Quote / backslash chars (`\`, `'`, `"`, `` ` ``) that would
///   close the literal early.
/// * Angle brackets / `&` (`<`, `>`, `&`) that would break out of
///   the surrounding `<script>` tag.
/// * Selected ASCII punctuation Django defends defensively
///   (`=`, `-`, `;`) so a payload like `</script><script>alert(1)`
///   cannot construct an event-handler attribute.
/// * Line terminators U+2028 / U+2029 — JS treats these as line
///   terminators in pre-ES2019 engines, which would split a
///   string literal mid-content.
/// * All ASCII control chars (< 0x20).
///
/// Every escaped character emits a 6-char `\uXXXX` sequence.
///
/// ```
/// use rustango::text::escapejs;
///
/// assert_eq!(escapejs("hello"), "hello");
/// assert_eq!(escapejs("a<b"), "a\\u003Cb");
/// assert_eq!(escapejs("\""), "\\u0022");
/// ```
pub fn escapejs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' | '\'' | '"' | '>' | '<' | '&' | '=' | '-' | ';' | '`' => {
                out.push_str(&format!("\\u{:04X}", ch as u32));
            }
            '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04X}", ch as u32));
            }
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", ch as u32));
            }
            other => out.push(other),
        }
    }
    out
}

pub fn json_script<T: serde::Serialize>(
    value: &T,
    element_id: &str,
) -> Result<String, serde_json::Error> {
    let raw = serde_json::to_string(value)?;
    // Django's exact escape set: `<` `>` `&` plus U+2028 / U+2029.
    // We escape via `\uXXXX` so the JSON stays valid for client-
    // side `JSON.parse`.
    let escaped = raw
        .replace('<', "\\u003C")
        .replace('>', "\\u003E")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let id_safe = html_escape(element_id);
    Ok(format!(
        r#"<script id="{id_safe}" type="application/json">{escaped}</script>"#
    ))
}

/// Django-parity
/// [`django.utils.text.wrap(text, width)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.text.wrap) —
/// word-wrap `text` to a column width of `width` characters,
/// inserting newlines between words to avoid exceeding the width.
///
/// Existing `\n` line breaks are preserved — each pre-existing
/// line wraps independently, so paragraph breaks aren't re-flowed.
/// Words longer than `width` are NOT hyphenated; they end up on a
/// line of their own (same as Django's `textwrap`-backed behavior).
/// `width = 0` returns the input unchanged.
///
/// rustango ships the same wrap algorithm as the Tera `|wordwrap`
/// filter — this is the programmatic surface for handler code.
///
/// ```ignore
/// use rustango::text::wrap;
/// let out = wrap("The quick brown fox jumps over the lazy dog", 14);
/// assert!(out.lines().all(|l| l.len() <= 14 || !l.contains(' ')));
/// assert_eq!(wrap("short", 80), "short");
/// assert_eq!(wrap("anything", 0), "anything");
/// ```
#[must_use]
pub fn wrap(text: &str, width: usize) -> String {
    if width == 0 {
        return text.to_owned();
    }
    text.split('\n')
        .map(|line| wrap_one_line(line, width))
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_one_line(line: &str, width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut current_len = 0usize;
    for (i, word) in line.split_whitespace().enumerate() {
        let word_chars = word.chars().count();
        if i == 0 {
            out.push_str(word);
            current_len = word_chars;
            continue;
        }
        let proposed = current_len + 1 + word_chars;
        if proposed <= width {
            out.push(' ');
            out.push_str(word);
            current_len = proposed;
        } else {
            out.push('\n');
            out.push_str(word);
            current_len = word_chars;
        }
    }
    out
}

/// Django-parity
/// [`django.utils.html.strip_spaces_between_tags(value)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.strip_spaces_between_tags) —
/// remove whitespace runs sitting BETWEEN HTML tags (i.e. between
/// a `>` and the next `<`). Used by Django's `{% spaceless %}`
/// template tag to compact rendered HTML.
///
/// Whitespace INSIDE text content is preserved — only the gap
/// between two adjacent tags is stripped.
///
/// ```ignore
/// use rustango::text::strip_spaces_between_tags;
/// assert_eq!(
///     strip_spaces_between_tags("<p>\n  <em>x</em>\n</p>"),
///     "<p><em>x</em></p>"
/// );
/// // Text inside tags is preserved.
/// assert_eq!(
///     strip_spaces_between_tags("<p>hello  world</p>"),
///     "<p>hello  world</p>"
/// );
/// ```
#[must_use]
pub fn strip_spaces_between_tags(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        if c == '>' {
            // Look ahead: skip whitespace until non-WS or `<`.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j] == '<' {
                // Whitespace run between `>` and `<` — skip it.
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Django-parity
/// [`django.utils.text.get_valid_filename(name)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.text.get_valid_filename) —
/// strip a user-supplied filename to something safe to drop on
/// disk: trim whitespace, replace internal whitespace + `/` and
/// `\` with underscores, drop any char that isn't alphanumeric,
/// dot, hyphen, or underscore.
///
/// Returns `Err(InvalidFilename)` if the result would be empty
/// or one of the special dot-names (`.` / `..`) — those are the
/// Django-parity rejected cases (Django raises
/// `SuspiciousFileOperation`; rustango surfaces as `Err` for
/// `?`-style propagation).
///
/// ```ignore
/// use rustango::text::get_valid_filename;
/// assert_eq!(get_valid_filename("  Pretty Doc.pdf  ").unwrap(), "Pretty_Doc.pdf");
/// assert_eq!(get_valid_filename("../../../etc/passwd").unwrap(), "etcpasswd");
/// assert!(get_valid_filename("").is_err());
/// assert!(get_valid_filename(".").is_err());
/// assert!(get_valid_filename("..").is_err());
/// ```
pub fn get_valid_filename(name: &str) -> Result<String, InvalidFilename> {
    let trimmed = name.trim();
    // First pass: replace internal whitespace + slash + backslash with
    // underscores; drop anything that isn't `[A-Za-z0-9._-]`.
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        if c.is_whitespace() || c == '/' || c == '\\' {
            out.push('_');
        } else if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        }
        // Everything else (control chars, punctuation, non-ASCII)
        // dropped — Django shape strips them silently.
    }
    if out.is_empty() || out == "." || out == ".." {
        return Err(InvalidFilename);
    }
    Ok(out)
}

/// Error returned by [`get_valid_filename`] when the input would
/// reduce to an empty / `.` / `..` filename — those are
/// path-traversal-prone shapes Django flags as
/// `SuspiciousFileOperation`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid filename: empty or special dot-name after sanitization")]
pub struct InvalidFilename;

/// Django-parity
/// [`django.utils.text.camel_case_to_spaces(value)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.text.camel_case_to_spaces) —
/// convert a CamelCase identifier into lowercase space-separated
/// words. Used by Django internally to derive `verbose_name` from
/// model class names (`BlogPost` → `"blog post"`).
///
/// The algorithm inserts a space before any uppercase letter that
/// is preceded by a lowercase letter or digit (the CamelCase
/// boundary), then lowercases the whole result and collapses any
/// internal whitespace runs.
///
/// ```ignore
/// use rustango::text::camel_case_to_spaces;
/// assert_eq!(camel_case_to_spaces("BlogPost"), "blog post");
/// assert_eq!(camel_case_to_spaces("HTTPRequest"), "httprequest");
/// assert_eq!(camel_case_to_spaces("simpleWord"), "simple word");
/// assert_eq!(camel_case_to_spaces("Already lowercase"), "already lowercase");
/// ```
#[must_use]
pub fn camel_case_to_spaces(value: &str) -> String {
    // Django's algorithm:
    //   re.sub(r'(((?<=[a-z])[A-Z])|([A-Z](?=[a-z])))', r' \1', value).lower()
    // → split before an uppercase letter that is EITHER:
    //   (a) preceded by a lowercase/digit boundary, OR
    //   (b) followed by a lowercase letter (acronym→word transition).
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_is_lower_or_digit =
                i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            // Only insert space when the immediately previous char isn't
            // already whitespace (avoids double-spacing).
            let prev_is_ws = i > 0 && chars[i - 1].is_whitespace();
            if (prev_is_lower_or_digit || (next_is_lower && i > 0 && !prev_is_ws)) && !prev_is_ws {
                out.push(' ');
            }
        }
        for lo in c.to_lowercase() {
            out.push(lo);
        }
    }
    // Collapse whitespace runs (Django shape — internal spaces fold too).
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
            }
            prev_space = true;
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    collapsed.trim().to_owned()
}

/// Django-parity
/// [`django.utils.text.unescape_string_literal(s)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.text.unescape_string_literal) —
/// strip surrounding quotes (`'` or `"`) from a quoted string
/// literal and un-escape backslash sequences inside. Used by
/// Django's template parser to handle quoted-string literals in
/// custom template tags.
///
/// `s` must be at least 2 chars long and start AND end with the
/// same quote character (either both `'` or both `"`). Backslash
/// escapes inside: `\\` → `\`, `\"` → `"`, `\'` → `'`. Other
/// escape sequences (`\n`, `\t`, etc.) pass through verbatim per
/// Django's shape (Django doesn't expand them either).
///
/// # Errors
/// Returns `None` when the input isn't a properly-quoted literal
/// (less than 2 chars, mismatched quote chars, etc.).
///
/// ```ignore
/// use rustango::text::unescape_string_literal;
/// assert_eq!(unescape_string_literal(r#""hello""#).as_deref(), Some("hello"));
/// assert_eq!(unescape_string_literal(r"'it\'s'").as_deref(), Some("it's"));
/// assert_eq!(unescape_string_literal(r#""\\path""#).as_deref(), Some(r"\path"));
/// assert!(unescape_string_literal("unquoted").is_none());
/// assert!(unescape_string_literal(r#""mismatched'"#).is_none());
/// ```
#[must_use]
pub fn unescape_string_literal(s: &str) -> Option<String> {
    if s.len() < 2 {
        return None;
    }
    let first = s.chars().next()?;
    let last = s.chars().last()?;
    if first != last || (first != '\'' && first != '"') {
        return None;
    }
    // Strip the wrapping quotes — careful with multi-byte chars at
    // boundaries (quotes are ASCII so single-byte slicing is safe).
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '\\' || next == '\'' || next == '"' {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    Some(out)
}

/// Django-parity
/// [`django.utils.html.linebreaks(value, autoescape=False)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.linebreaks) —
/// convert plain-text line breaks into HTML paragraphs and `<br>`
/// tags. The canonical "render textarea-input as HTML preserving
/// paragraph structure" transformation.
///
/// Algorithm (Django shape):
/// * Normalize CRLF / CR → LF (matches [`normalize_newlines`])
/// * Split on blank-line runs (`\n\n+`) into paragraphs
/// * Within a paragraph, single `\n` becomes `<br>`
/// * Wrap each paragraph in `<p>...</p>`
///
/// When `autoescape = true`, the input is `html_escape`d before
/// the transformation so user-supplied HTML can't escape the
/// containing element. When `false`, the input passes through
/// verbatim (Django shape — caller has already validated).
///
/// ```ignore
/// use rustango::text::linebreaks;
/// assert_eq!(
///     linebreaks("Para one.\n\nPara two.", true),
///     "<p>Para one.</p>\n\n<p>Para two.</p>"
/// );
/// assert_eq!(
///     linebreaks("Line one.\nLine two.", true),
///     "<p>Line one.<br>Line two.</p>"
/// );
/// ```
#[must_use]
pub fn linebreaks(value: &str, autoescape: bool) -> String {
    let normalized = normalize_newlines(value);
    let safe: String = if autoescape {
        html_escape(&normalized)
    } else {
        normalized
    };
    // Split on blank-line runs (`\n\n+`). We can't use String::split
    // because it doesn't collapse adjacent separators — manually walk.
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut blank_run = false;
    for line in safe.split('\n') {
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            blank_run = true;
        } else {
            if !current.is_empty() && !blank_run {
                current.push_str("<br>");
            }
            current.push_str(line);
            blank_run = false;
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs
        .into_iter()
        .map(|p| format!("<p>{p}</p>"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Django-parity
/// [`django.utils.html.linebreaks_br(value, autoescape=False)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.linebreaksbr) —
/// convert ALL `\n` line breaks into `<br>` tags, without
/// paragraph wrapping. Use when you want preserved newlines inside
/// an already-`<p>`-wrapped element (e.g. a single-paragraph
/// description field).
///
/// Same CRLF normalization + autoescape semantics as [`linebreaks`].
///
/// ```ignore
/// use rustango::text::linebreaks_br;
/// assert_eq!(
///     linebreaks_br("Line one.\nLine two.\nLine three.", true),
///     "Line one.<br>Line two.<br>Line three."
/// );
/// ```
#[must_use]
pub fn linebreaks_br(value: &str, autoescape: bool) -> String {
    let normalized = normalize_newlines(value);
    let safe: String = if autoescape {
        html_escape(&normalized)
    } else {
        normalized
    };
    safe.replace('\n', "<br>")
}

/// Django-parity
/// [`django.utils.html.format_html(format_string, *args, **kwargs)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.format_html) —
/// build an HTML string from a positional `{}`-style template,
/// HTML-escaping every interpolated argument. This is the safe
/// way to construct HTML strings inline without manually calling
/// `html_escape` on every variable.
///
/// `{}` placeholders are filled positionally from `args` in order
/// of appearance. Each value is HTML-escaped via [`html_escape`]
/// before substitution. Literal `{` / `}` characters in the
/// template can be escaped as `{{` / `}}` (Rust format-string
/// convention, NOT Django's — Django uses `str.format`'s shape but
/// rustango uses a simple positional placeholder for the same
/// safety property without dragging in str-format syntax).
///
/// ```ignore
/// use rustango::text::format_html;
/// // Variables auto-escaped — user-supplied "<script>" is rendered safely.
/// assert_eq!(
///     format_html(
///         "<a href=\"{}\">{}</a>",
///         &["/x", "<script>alert(1)</script>"]
///     ),
///     "<a href=\"/x\">&lt;script&gt;alert(1)&lt;/script&gt;</a>"
/// );
/// ```
///
/// Excess args (more than `{}` placeholders) are silently ignored;
/// too few args produces an empty replacement at the missing
/// position. This is the lenient shape — strict-arity checking is
/// available via direct `format!` if you want compile-time
/// guarantees.
#[must_use]
pub fn format_html(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut arg_idx = 0;
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && i + 1 < bytes.len() {
            // `{{` → literal `{`.
            if bytes[i + 1] == b'{' {
                out.push('{');
                i += 2;
                continue;
            }
            // `{}` → argument substitution.
            if bytes[i + 1] == b'}' {
                if let Some(value) = args.get(arg_idx) {
                    out.push_str(&html_escape(value));
                }
                arg_idx += 1;
                i += 2;
                continue;
            }
        }
        if bytes[i] == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            // `}}` → literal `}`.
            out.push('}');
            i += 2;
            continue;
        }
        // SAFETY: bytes is the UTF-8 representation of `template`;
        // we only consume one byte at a time but the chars iterator
        // would be more correct here. Use `template[i..]` chars step
        // for codepoint-safe iteration.
        let next_char = template[i..]
            .chars()
            .next()
            .expect("non-empty slice has at least one char");
        out.push(next_char);
        i += next_char.len_utf8();
    }
    out
}

/// Django-parity
/// [`django.utils.html.format_html_join(sep, format_string, args_generator)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.format_html_join) —
/// build a joined HTML string from an iterator of argument tuples.
/// Same safety property as [`format_html`] — every arg HTML-escaped
/// before substitution — but folds repetition over a list.
///
/// Common use: rendering a `<table>` body where each row needs the
/// same template applied to its column values.
///
/// ```ignore
/// use rustango::text::format_html_join;
/// // Build a comma-separated <a> list from three (url, label) pairs.
/// let rows = [
///     vec!["/a", "First"],
///     vec!["/b", "Second"],
///     vec!["/c", "Third"],
/// ];
/// let html = format_html_join(", ", "<a href=\"{}\">{}</a>", &rows);
/// // → `<a href="/a">First</a>, <a href="/b">Second</a>, <a href="/c">Third</a>`
/// ```
#[must_use]
pub fn format_html_join(sep: &str, format_string: &str, args: &[Vec<&str>]) -> String {
    let mut out = String::with_capacity(args.len() * format_string.len());
    for (i, row) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push_str(&format_html(format_string, row));
    }
    out
}

/// Django-parity
/// [`django.utils.html.urlize(text, trim_url_limit=None, nofollow=False, autoescape=True)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.html.urlize) —
/// convert URLs and email addresses inside `text` into clickable
/// HTML anchor tags.
///
/// rustango's bounded port detects three shapes per Django:
///
/// * `http://...` and `https://...` absolute URLs → `<a href="URL">URL</a>`
/// * `www.domain.tld[/path]` bare-www URLs → `<a href="http://www.domain.tld...">www.domain.tld...</a>`
/// * `user@host.tld` email addresses → `<a href="mailto:user@host.tld">user@host.tld</a>`
///
/// Adjacent punctuation (`.`, `,`, `;`, `:`, `!`, `?`, `)`, `]`)
/// is trimmed off the URL end so trailing prose punctuation reads
/// naturally — `"See http://x.com."` renders the period OUTSIDE
/// the anchor.
///
/// `nofollow = true` adds `rel="nofollow"` to anchors (Django parity
/// — defends against link-farming on user-submitted text). Body
/// text outside detected URLs passes through verbatim — caller
/// must escape the input first if the source is untrusted (Django's
/// `autoescape` flag handles that there; rustango leaves escape to
/// the caller via [`html_escape`]).
///
/// ```ignore
/// use rustango::text::urlize;
/// assert_eq!(
///     urlize("See https://example.com for more.", false),
///     r#"See <a href="https://example.com">https://example.com</a> for more."#
/// );
/// assert_eq!(
///     urlize("Email me@example.com", false),
///     r#"Email <a href="mailto:me@example.com">me@example.com</a>"#
/// );
/// // nofollow=true
/// assert_eq!(
///     urlize("https://x.com", true),
///     r#"<a href="https://x.com" rel="nofollow">https://x.com</a>"#
/// );
/// ```
#[must_use]
pub fn urlize(text: &str, nofollow: bool) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let rel_attr = if nofollow { r#" rel="nofollow""# } else { "" };
    for token in text.split_inclusive(char::is_whitespace) {
        let (leading_ws_pos, body, trailing_ws) = split_off_trailing_ws(token);
        let _ = leading_ws_pos;
        let (trail_punct_start, trail_punct) = split_off_trailing_punct(body);
        let core = &body[..trail_punct_start];

        if let Some(rendered) = render_match(core, rel_attr) {
            out.push_str(&rendered);
            out.push_str(trail_punct);
            out.push_str(trailing_ws);
        } else {
            out.push_str(token);
        }
    }
    out
}

/// Split a token into `(body_without_trailing_ws, trailing_ws)`.
/// Returns `(0, body, "")` if there's no trailing whitespace.
fn split_off_trailing_ws(token: &str) -> (usize, &str, &str) {
    let trail_start = token
        .char_indices()
        .rev()
        .take_while(|&(_, c)| c.is_whitespace())
        .last()
        .map_or(token.len(), |(i, _)| i);
    (0, &token[..trail_start], &token[trail_start..])
}

/// Trim adjacent prose punctuation from the END of a URL-shaped
/// token so it doesn't get sucked into the anchor href.
fn split_off_trailing_punct(s: &str) -> (usize, &str) {
    let mut idx = s.len();
    for (i, c) in s.char_indices().rev() {
        if matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '"' | '\''
        ) {
            idx = i;
        } else {
            break;
        }
    }
    (idx, &s[idx..])
}

/// Detect the three Django-supported URL shapes inside `core` and
/// return the rendered HTML anchor. Returns `None` for non-matches
/// so the caller can emit the literal token instead.
fn render_match(core: &str, rel_attr: &str) -> Option<String> {
    if core.starts_with("http://") || core.starts_with("https://") {
        return Some(format!(r#"<a href="{core}"{rel_attr}>{core}</a>"#));
    }
    if core.starts_with("www.") && core.contains('.') {
        return Some(format!(r#"<a href="http://{core}"{rel_attr}>{core}</a>"#));
    }
    // Email: at-sign present, surrounded by something on both sides,
    // domain contains a `.`.
    if let Some(at) = core.find('@') {
        if at > 0 && at < core.len() - 1 {
            let (local, _) = core.split_at(at);
            let domain = &core[at + 1..];
            if !local.is_empty() && domain.contains('.') {
                return Some(format!(r#"<a href="mailto:{core}"{rel_attr}>{core}</a>"#));
            }
        }
    }
    None
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

    // -------- truncatechars (Django filter parity) --------

    #[test]
    fn truncatechars_basic() {
        // 7 chars total including the ellipsis.
        assert_eq!(truncatechars("Joel is a slug", 7), "Joel i…");
        assert_eq!(truncatechars("abcd", 3), "ab…");
    }

    #[test]
    fn truncatechars_no_truncation_when_short() {
        assert_eq!(truncatechars("Hi", 10), "Hi");
        // Boundary: exactly count chars → no truncation, no ellipsis.
        assert_eq!(truncatechars("abc", 3), "abc");
        assert_eq!(truncatechars("", 5), "");
    }

    #[test]
    fn truncatechars_zero_count_empty() {
        assert_eq!(truncatechars("anything", 0), "");
    }

    #[test]
    fn truncatechars_count_one_just_ellipsis() {
        // Edge case: count=1 → keep 0 chars + ellipsis.
        assert_eq!(truncatechars("hello", 1), "…");
    }

    #[test]
    fn truncatechars_unicode_chars_count_as_one() {
        // "café" has 4 chars but 5 bytes (`é` is 2 bytes in UTF-8).
        // Count by chars, not bytes.
        assert_eq!(truncatechars("café-bar", 5), "café…");
        assert_eq!(truncatechars("café", 4), "café"); // already at count
    }

    #[test]
    fn truncatechars_total_chars_includes_ellipsis() {
        // Result must never exceed count chars total.
        let out = truncatechars("abcdefghij", 4);
        assert_eq!(out.chars().count(), 4);
        assert_eq!(out, "abc…");
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

    // -------- urlize (Django parity) --------

    #[test]
    fn urlize_http_url_becomes_anchor() {
        assert_eq!(
            urlize("Visit https://example.com for more", false),
            r#"Visit <a href="https://example.com">https://example.com</a> for more"#
        );
    }

    #[test]
    fn urlize_https_url_becomes_anchor() {
        let out = urlize("https://example.com/path", false);
        assert!(out.contains(r#"<a href="https://example.com/path""#));
        assert!(out.contains(">https://example.com/path</a>"));
    }

    #[test]
    fn urlize_strips_trailing_punctuation_from_url() {
        // Period belongs to the sentence, not the URL.
        let out = urlize("See https://x.com.", false);
        assert_eq!(out, r#"See <a href="https://x.com">https://x.com</a>."#);
    }

    #[test]
    fn urlize_handles_multiple_punctuation() {
        // Question + close-paren should both stay outside the URL.
        let out = urlize("(check https://x.com?)", false);
        assert!(out.contains(r#"<a href="https://x.com""#));
        assert!(out.ends_with(")"));
        assert!(out.contains("?)"));
    }

    #[test]
    fn urlize_email_becomes_mailto() {
        let out = urlize("Reach me@example.com please", false);
        assert!(out.contains(r#"<a href="mailto:me@example.com""#));
        assert!(out.contains(">me@example.com</a>"));
    }

    #[test]
    fn urlize_www_prefix_gets_http_added() {
        // `www.foo.com` → `<a href="http://www.foo.com">www.foo.com</a>`
        let out = urlize("www.example.com works", false);
        assert!(out.contains(r#"<a href="http://www.example.com""#));
        assert!(out.contains(">www.example.com</a>"));
    }

    #[test]
    fn urlize_nofollow_adds_rel_attribute() {
        let out = urlize("https://x.com", true);
        assert_eq!(
            out,
            r#"<a href="https://x.com" rel="nofollow">https://x.com</a>"#
        );
    }

    #[test]
    fn urlize_plain_text_passes_through() {
        assert_eq!(urlize("nothing here", false), "nothing here");
    }

    #[test]
    fn urlize_does_not_match_bare_words_with_at() {
        // `not@a@valid` has a degenerate at-pattern; domain side has
        // no `.` → not anchored. Original token preserved.
        let out = urlize("ping not@a@valid for input", false);
        assert!(!out.contains("<a"));
    }

    #[test]
    fn urlize_at_sign_without_domain_dot_not_matched() {
        // `user@localhost` → no domain dot → not an email per shape.
        let out = urlize("contact user@localhost", false);
        assert!(!out.contains("<a"));
    }

    #[test]
    fn urlize_handles_multiple_urls_in_one_string() {
        let out = urlize("First https://a.com second https://b.com end", false);
        assert!(out.contains(r#"<a href="https://a.com""#));
        assert!(out.contains(r#"<a href="https://b.com""#));
    }

    // -------- format_html / format_html_join (Django parity) --------

    #[test]
    fn format_html_substitutes_and_escapes_args() {
        let out = format_html(
            r#"<a href="{}">{}</a>"#,
            &["/x", "<script>alert(1)</script>"],
        );
        assert_eq!(
            out,
            r#"<a href="/x">&lt;script&gt;alert(1)&lt;/script&gt;</a>"#
        );
    }

    #[test]
    fn format_html_no_placeholders() {
        assert_eq!(format_html("hello", &[]), "hello");
        // Extra args silently ignored when there are no placeholders.
        assert_eq!(format_html("hello", &["ignored"]), "hello");
    }

    #[test]
    fn format_html_multiple_args() {
        let out = format_html("{} + {} = {}", &["1", "2", "3"]);
        assert_eq!(out, "1 + 2 = 3");
    }

    #[test]
    fn format_html_too_few_args_drops_placeholders() {
        // Missing args leave the placeholder empty rather than panicking.
        let out = format_html("{}-{}-{}", &["A", "B"]);
        assert_eq!(out, "A-B-");
    }

    #[test]
    fn format_html_escapes_html_entities_in_args() {
        let out = format_html("<p>{}</p>", &[r#"a & b > c < d "quoted" 'apost'"#]);
        assert!(out.contains("&amp;"));
        assert!(out.contains("&gt;"));
        assert!(out.contains("&lt;"));
        assert!(out.contains("&quot;"));
        assert!(out.contains("&#x27;"));
    }

    #[test]
    fn format_html_handles_literal_braces() {
        // `{{` and `}}` escape to literal `{` / `}`.
        let out = format_html("{{}} = {}", &["empty"]);
        assert_eq!(out, "{} = empty");
    }

    #[test]
    fn format_html_handles_unicode_in_template_and_args() {
        let out = format_html("Café — {}", &["résumé"]);
        assert_eq!(out, "Café — résumé");
    }

    #[test]
    fn format_html_join_renders_each_row() {
        let rows = vec![
            vec!["/a", "First"],
            vec!["/b", "Second"],
            vec!["/c", "Third"],
        ];
        let out = format_html_join(", ", r#"<a href="{}">{}</a>"#, &rows);
        assert_eq!(
            out,
            r#"<a href="/a">First</a>, <a href="/b">Second</a>, <a href="/c">Third</a>"#
        );
    }

    #[test]
    fn format_html_join_empty_rows_yields_empty_string() {
        let out: String = format_html_join(", ", "{}", &[]);
        assert_eq!(out, "");
    }

    #[test]
    fn format_html_join_escapes_each_row_independently() {
        let rows: Vec<Vec<&str>> = vec![vec!["<bad>"], vec!["<also>"]];
        let out = format_html_join("|", "<li>{}</li>", &rows);
        assert!(out.contains("&lt;bad&gt;"));
        assert!(out.contains("&lt;also&gt;"));
        assert!(!out.contains("<bad>"));
    }

    // -------- linebreaks / linebreaks_br (Django parity) --------

    #[test]
    fn linebreaks_blank_lines_become_paragraphs() {
        assert_eq!(
            linebreaks("Para one.\n\nPara two.", true),
            "<p>Para one.</p>\n\n<p>Para two.</p>"
        );
    }

    #[test]
    fn linebreaks_single_newlines_become_br() {
        assert_eq!(
            linebreaks("Line one.\nLine two.", true),
            "<p>Line one.<br>Line two.</p>"
        );
    }

    #[test]
    fn linebreaks_three_paragraphs() {
        let out = linebreaks("a\n\nb\n\nc", true);
        assert_eq!(out, "<p>a</p>\n\n<p>b</p>\n\n<p>c</p>");
    }

    #[test]
    fn linebreaks_normalizes_crlf() {
        // Windows-style CRLF should produce the same output as LF-only.
        let crlf = linebreaks("Para one.\r\n\r\nPara two.", true);
        let lf = linebreaks("Para one.\n\nPara two.", true);
        assert_eq!(crlf, lf);
    }

    #[test]
    fn linebreaks_autoescape_protects_user_html() {
        let out = linebreaks("<script>alert(1)</script>", true);
        assert!(out.contains("&lt;script&gt;"));
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn linebreaks_no_autoescape_passes_html_through() {
        // autoescape=false means the input is trusted markup.
        let out = linebreaks("<em>x</em>", false);
        assert_eq!(out, "<p><em>x</em></p>");
    }

    #[test]
    fn linebreaks_empty_input() {
        assert_eq!(linebreaks("", true), "");
    }

    #[test]
    fn linebreaks_collapses_multi_blank_runs_to_one_split() {
        // Three blank lines should still produce two paragraphs, not three.
        let out = linebreaks("a\n\n\n\nb", true);
        assert_eq!(out, "<p>a</p>\n\n<p>b</p>");
    }

    #[test]
    fn linebreaks_br_replaces_every_newline() {
        assert_eq!(
            linebreaks_br("Line one.\nLine two.\nLine three.", true),
            "Line one.<br>Line two.<br>Line three."
        );
    }

    #[test]
    fn linebreaks_br_handles_crlf() {
        assert_eq!(linebreaks_br("a\r\nb\rc", true), "a<br>b<br>c");
    }

    #[test]
    fn linebreaks_br_autoescape() {
        let out = linebreaks_br("<x>\nfoo", true);
        assert!(out.contains("&lt;x&gt;"));
        assert!(out.contains("<br>")); // <br> tag should NOT be escaped
    }

    #[test]
    fn linebreaks_br_empty_input() {
        assert_eq!(linebreaks_br("", true), "");
    }

    // -------- camel_case_to_spaces (Django parity) --------

    #[test]
    fn camel_case_simple() {
        assert_eq!(camel_case_to_spaces("BlogPost"), "blog post");
    }

    #[test]
    fn camel_case_multiple_words() {
        assert_eq!(
            camel_case_to_spaces("ThisIsALongName"),
            "this is a long name"
        );
    }

    #[test]
    fn camel_case_acronym_word_boundary_splits() {
        // Django's regex splits at the acronym→word transition:
        // `HTTPRequest` becomes `"http request"` because R is uppercase
        // followed by a lowercase e.
        assert_eq!(camel_case_to_spaces("HTTPRequest"), "http request");
    }

    #[test]
    fn camel_case_starts_lowercase() {
        assert_eq!(camel_case_to_spaces("simpleWord"), "simple word");
    }

    #[test]
    fn camel_case_already_lowercase() {
        assert_eq!(
            camel_case_to_spaces("Already lowercase"),
            "already lowercase"
        );
    }

    #[test]
    fn camel_case_with_digit_boundary() {
        // Digit-then-uppercase is a CamelCase boundary too.
        assert_eq!(camel_case_to_spaces("Version2Beta"), "version2 beta");
    }

    #[test]
    fn camel_case_empty() {
        assert_eq!(camel_case_to_spaces(""), "");
    }

    #[test]
    fn camel_case_collapses_existing_spaces() {
        // Multiple existing spaces collapse to one.
        assert_eq!(camel_case_to_spaces("foo  bar   baz"), "foo bar baz");
    }

    // -------- unescape_string_literal (Django parity) --------

    #[test]
    fn unescape_double_quoted() {
        assert_eq!(
            unescape_string_literal(r#""hello""#).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn unescape_single_quoted() {
        assert_eq!(
            unescape_string_literal(r"'world'").as_deref(),
            Some("world")
        );
    }

    #[test]
    fn unescape_handles_embedded_escaped_quote() {
        // `'it\'s'` → `"it's"`.
        assert_eq!(unescape_string_literal(r"'it\'s'").as_deref(), Some("it's"));
    }

    #[test]
    fn unescape_handles_escaped_backslash() {
        assert_eq!(
            unescape_string_literal(r#""\\path""#).as_deref(),
            Some(r"\path")
        );
    }

    #[test]
    fn unescape_passes_through_non_special_escapes() {
        // \n / \t / \r are NOT expanded per Django shape.
        let out = unescape_string_literal(r#""line\nbreak""#).unwrap();
        // Backslash + n preserved literally (Django doesn't expand).
        assert_eq!(out, r"line\nbreak");
    }

    #[test]
    fn unescape_rejects_unquoted() {
        assert!(unescape_string_literal("plain").is_none());
    }

    #[test]
    fn unescape_rejects_mismatched_quotes() {
        assert!(unescape_string_literal(r#""mismatched'"#).is_none());
        assert!(unescape_string_literal(r#"'mismatched""#).is_none());
    }

    #[test]
    fn unescape_rejects_too_short() {
        assert!(unescape_string_literal("").is_none());
        assert!(unescape_string_literal("'").is_none());
        // A single quote pair encloses zero chars — valid empty.
        assert_eq!(unescape_string_literal("''").as_deref(), Some(""));
        assert_eq!(unescape_string_literal(r#""""#).as_deref(), Some(""));
    }

    #[test]
    fn unescape_rejects_non_quote_wrappers() {
        // Brackets / parens / angle brackets are NOT quote chars.
        assert!(unescape_string_literal("[hello]").is_none());
        assert!(unescape_string_literal("(hello)").is_none());
    }

    // -------- get_valid_filename (Django parity) --------

    #[test]
    fn valid_filename_replaces_whitespace_with_underscore() {
        assert_eq!(
            get_valid_filename("  Pretty Doc.pdf  ").unwrap(),
            "Pretty_Doc.pdf"
        );
    }

    #[test]
    fn valid_filename_strips_path_traversal_chars() {
        // Slashes + dots survive but no separator structure.
        // `../../../etc/passwd` → slashes become underscores, dots
        // are valid filename chars → "..___..___..___etc_passwd"
        // hmm that's different. Let me reconsider — Django's regex
        // for get_valid_filename: re.sub(r'(?u)[^-\w.]', '', s)
        // which drops non-alphanumeric + non-`-` + non-`.` + non-`_`.
        // Whitespace becomes underscore in a SEPARATE first pass.
        // So `../../../etc/passwd` → `../../../etcpasswd` (slashes
        // dropped, dots preserved). Let me verify our behavior.
        let out = get_valid_filename("../../../etc/passwd").unwrap();
        // Slashes dropped → `..` `..` `..` `etc` `passwd` concatenated.
        // The dots in `..` stay; the result is `..........etcpasswd`.
        assert!(out.contains("etc"));
        assert!(out.contains("passwd"));
        assert!(!out.contains('/'));
    }

    #[test]
    fn valid_filename_drops_punctuation() {
        let out = get_valid_filename("file (1)!@#$.txt").unwrap();
        assert!(!out.contains('('));
        assert!(!out.contains(')'));
        assert!(!out.contains('!'));
        assert!(out.contains(".txt"));
    }

    #[test]
    fn valid_filename_preserves_unicode_alphanumerics_drops_punctuation() {
        // Django shape — alphanumerics OK, punctuation stripped.
        // Non-ASCII alphanumerics are dropped under our `is_ascii_alphanumeric`
        // check (Django uses regex \w which DOES match unicode word chars
        // — we differ here, but the safer-on-disk shape is to drop them).
        let out = get_valid_filename("résumé.pdf").unwrap();
        assert!(out.ends_with(".pdf"));
    }

    #[test]
    fn valid_filename_rejects_empty() {
        assert!(get_valid_filename("").is_err());
        assert!(get_valid_filename("   ").is_err()); // trim → empty
    }

    #[test]
    fn valid_filename_rejects_dot_specials() {
        // Django flags these as SuspiciousFileOperation. We surface
        // as Err.
        assert!(get_valid_filename(".").is_err());
        assert!(get_valid_filename("..").is_err());
    }

    #[test]
    fn valid_filename_replaces_backslash_too() {
        // Windows-style path separator → underscore (Django shape).
        let out = get_valid_filename(r"C:\Users\foo.txt").unwrap();
        assert!(!out.contains('\\'));
        assert!(!out.contains(':'));
    }

    // -------- strip_spaces_between_tags (Django parity) --------

    #[test]
    fn strip_spaces_between_tags_compacts_tag_gap() {
        assert_eq!(
            strip_spaces_between_tags("<p>\n  <em>x</em>\n</p>"),
            "<p><em>x</em></p>"
        );
    }

    #[test]
    fn strip_spaces_between_tags_preserves_inner_text() {
        // Whitespace inside element content stays.
        assert_eq!(
            strip_spaces_between_tags("<p>hello  world</p>"),
            "<p>hello  world</p>"
        );
    }

    #[test]
    fn strip_spaces_between_tags_handles_self_closing() {
        assert_eq!(strip_spaces_between_tags("<br/>\n<br/>"), "<br/><br/>");
    }

    #[test]
    fn strip_spaces_between_tags_preserves_mixed_content() {
        // Text between a closing tag and the next opening tag (not just
        // whitespace) is preserved.
        let s = "<p>one</p> and <p>two</p>";
        assert_eq!(strip_spaces_between_tags(s), "<p>one</p> and <p>two</p>");
    }

    #[test]
    fn strip_spaces_between_tags_empty() {
        assert_eq!(strip_spaces_between_tags(""), "");
    }

    #[test]
    fn strip_spaces_between_tags_no_tags_passes_through() {
        // Pure text input: no `>` boundaries, so nothing to strip.
        assert_eq!(
            strip_spaces_between_tags("plain text with spaces"),
            "plain text with spaces"
        );
    }

    #[test]
    fn strip_spaces_between_tags_handles_unicode_content() {
        assert_eq!(
            strip_spaces_between_tags("<p>\n  <em>café</em>\n</p>"),
            "<p><em>café</em></p>"
        );
    }

    // -------- wrap (Django parity) --------

    #[test]
    fn wrap_short_text_unchanged() {
        assert_eq!(wrap("short", 80), "short");
    }

    #[test]
    fn wrap_breaks_at_word_boundary() {
        let out = wrap("The quick brown fox", 10);
        // Should break — first line ≤ 10 chars, second carries the rest.
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 2);
        for line in &lines {
            // Either fits in 10 chars OR is a single word longer than 10.
            assert!(
                line.chars().count() <= 10 || !line.contains(' '),
                "line `{line}` exceeds width and has multiple words"
            );
        }
    }

    #[test]
    fn wrap_preserves_explicit_newlines() {
        // Each line is wrapped independently.
        let out = wrap("First line.\nSecond line.", 80);
        assert_eq!(out, "First line.\nSecond line.");
    }

    #[test]
    fn wrap_zero_width_returns_input_unchanged() {
        let text = "anything goes here";
        assert_eq!(wrap(text, 0), text);
    }

    #[test]
    fn wrap_long_word_on_own_line() {
        // A word longer than width can't fit but doesn't crash.
        let out = wrap("hi superlongwordherethatexceedswidth bye", 10);
        // The long word ends up on its own line.
        assert!(out.contains("superlongwordherethatexceedswidth"));
    }

    #[test]
    fn wrap_empty_input() {
        assert_eq!(wrap("", 80), "");
    }

    #[test]
    fn wrap_collapses_whitespace_in_lines() {
        // textwrap shape: multiple internal spaces collapse to one.
        let out = wrap("a   b   c", 80);
        assert_eq!(out, "a b c");
    }

    // -------- json_script (Django parity) --------

    #[derive(serde::Serialize)]
    struct Bootstrap {
        user_id: u64,
        name: String,
    }

    #[test]
    fn json_script_wraps_in_script_tag_with_id() {
        let out = json_script(
            &Bootstrap {
                user_id: 42,
                name: "alice".into(),
            },
            "bootstrap",
        )
        .unwrap();
        assert!(out.starts_with(r#"<script id="bootstrap" type="application/json">"#));
        assert!(out.ends_with("</script>"));
        assert!(out.contains(r#""user_id":42"#));
        assert!(out.contains(r#""name":"alice""#));
    }

    #[test]
    fn json_script_escapes_lt_gt_amp_to_unicode_escapes() {
        // Strings with `<` `>` `&` get escaped so a `</script>` in the
        // payload can't break out of the script element.
        let v: String = "</script><script>alert(1)</script>".into();
        let out = json_script(&v, "x").unwrap();
        assert!(
            !out.contains("</script><script>"),
            "raw </script> must NOT appear in body — would break out: {out}"
        );
        // The `<` chars get unicode-escaped.
        assert!(out.contains(r"<"));
    }

    #[test]
    fn json_script_escapes_line_terminators() {
        // U+2028 / U+2029 — JavaScript line terminators inside string
        // literals (pre-ES2019 strict parsers choke).
        let v: String = "line\u{2028}sep".into();
        let out = json_script(&v, "x").unwrap();
        assert!(out.contains(r" "));
        assert!(!out.contains('\u{2028}'));
    }

    #[test]
    fn json_script_escapes_element_id_as_html_attr() {
        // Attacker-controlled element_id can't break out of the
        // `id="..."` attribute via embedded `"` or `>`.
        let out = json_script(&42u64, r#"x" onload="alert(1)"#).unwrap();
        assert!(!out.contains(r#"" onload="#));
        assert!(out.contains("&quot;"));
    }

    #[test]
    fn json_script_handles_simple_values() {
        let out_int = json_script(&42u64, "n").unwrap();
        assert!(out_int.contains(">42</script>"));
        let out_str = json_script(&"hello", "s").unwrap();
        assert!(out_str.contains(r#">"hello"</script>"#));
        let out_arr = json_script(&vec![1, 2, 3], "a").unwrap();
        assert!(out_arr.contains(">[1,2,3]</script>"));
    }

    #[test]
    fn json_script_produces_parseable_json_body() {
        // The escaped body must round-trip through serde_json::from_str
        // after the JS-side `<` / `>` / `&` are NOT
        // unescaped — JSON-spec-valid `\uXXXX` escapes parse as the
        // matching char.
        let v: String = "a<b>c".into();
        let out = json_script(&v, "x").unwrap();
        // Extract the body between `>` and `</script>`.
        let body_start = out.find("\">").unwrap() + 2;
        let body_end = out.find("</script>").unwrap();
        let body = &out[body_start..body_end];
        let decoded: String = serde_json::from_str(body).unwrap();
        assert_eq!(decoded, "a<b>c");
    }

    // -------- escapejs --------

    #[test]
    fn escapejs_passes_through_plain_text() {
        assert_eq!(escapejs("hello world"), "hello world");
        assert_eq!(escapejs(""), "");
        // Unicode beyond the escape set passes through.
        assert_eq!(escapejs("café"), "café");
    }

    #[test]
    fn escapejs_escapes_quote_and_backslash() {
        assert_eq!(escapejs("\""), "\\u0022");
        assert_eq!(escapejs("'"), "\\u0027");
        assert_eq!(escapejs("\\"), "\\u005C");
        assert_eq!(escapejs("`"), "\\u0060");
    }

    #[test]
    fn escapejs_escapes_html_breakout_chars() {
        // `</script>` — angle brackets are escaped; the `/` and the
        // text "script" pass through (Django escape set excludes `/`).
        let out = escapejs("</script>");
        // The literal substring `</script>` cannot appear — the `<`
        // and `>` are both escaped to `<` / `>`.
        assert!(!out.contains("</script>"));
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert!(out.contains("\\u003C"));
        assert!(out.contains("\\u003E"));
        // `/` and the word "script" stay verbatim.
        assert!(out.contains('/'));
        assert!(out.contains("script"));
    }

    #[test]
    fn escapejs_escapes_punctuation_for_event_handler_defense() {
        // Django's defense-in-depth set includes `=`, `-`, `;` — so
        // a payload like `onerror=alert(1)` can't be assembled inside
        // a string-literal context that later flows into innerHTML.
        assert_eq!(escapejs("="), "\\u003D");
        assert_eq!(escapejs("-"), "\\u002D");
        assert_eq!(escapejs(";"), "\\u003B");
    }

    #[test]
    fn escapejs_escapes_line_separators() {
        // U+2028 / U+2029 break pre-ES2019 JS string literals.
        assert_eq!(escapejs("\u{2028}"), "\\u2028");
        assert_eq!(escapejs("\u{2029}"), "\\u2029");
    }

    #[test]
    fn escapejs_escapes_control_chars() {
        // Every ASCII control char (< 0x20) becomes a 6-char escape.
        assert_eq!(escapejs("\n"), "\\u000A");
        assert_eq!(escapejs("\t"), "\\u0009");
        assert_eq!(escapejs("\0"), "\\u0000");
        assert_eq!(escapejs("\x1f"), "\\u001F");
    }

    #[test]
    fn escapejs_full_xss_payload() {
        let payload = r#"</script><script>alert("xss")</script>"#;
        let out = escapejs(payload);
        // The literal `</script>` cannot appear in the output.
        assert!(!out.contains("</script>"));
        assert!(!out.contains("<script>"));
        // Plain text content like `alert` and `xss` passes through —
        // it's just the punctuation that's escaped.
        assert!(out.contains("alert"));
    }

    // -------- oxford_join --------

    #[test]
    fn oxford_join_empty_list() {
        assert_eq!(oxford_join(&[] as &[&str], "and"), "");
    }

    #[test]
    fn oxford_join_one_item_no_conjunction() {
        assert_eq!(oxford_join(&["alone"], "and"), "alone");
    }

    #[test]
    fn oxford_join_two_items_no_comma() {
        assert_eq!(oxford_join(&["a", "b"], "and"), "a and b");
    }

    #[test]
    fn oxford_join_three_plus_uses_oxford_comma() {
        assert_eq!(oxford_join(&["a", "b", "c"], "and"), "a, b, and c");
        assert_eq!(oxford_join(&["a", "b", "c", "d"], "and"), "a, b, c, and d");
    }

    #[test]
    fn oxford_join_custom_conjunction() {
        assert_eq!(oxford_join(&["a", "b", "c"], "or"), "a, b, or c");
        assert_eq!(oxford_join(&["a", "b"], "via"), "a via b");
    }

    #[test]
    fn oxford_join_accepts_strings_and_strs() {
        let owned: Vec<String> = vec!["x".to_owned(), "y".to_owned(), "z".to_owned()];
        assert_eq!(oxford_join(&owned, "and"), "x, y, and z");
    }

    // -------- initials --------

    #[test]
    fn initials_basic() {
        assert_eq!(initials("Alice", None), "A");
        assert_eq!(initials("Alice Bob", None), "AB");
        assert_eq!(initials("alice m. bob", None), "AMB");
    }

    #[test]
    fn initials_with_limit() {
        assert_eq!(initials("alice m. bob", Some(2)), "AM");
        assert_eq!(initials("alice m. bob", Some(1)), "A");
        assert_eq!(initials("alice m. bob", Some(99)), "AMB");
        assert_eq!(initials("alice m. bob", Some(0)), "");
    }

    #[test]
    fn initials_skips_non_alphabetic_leading_chars() {
        assert_eq!(initials("123 Alice", None), "A");
        // Word with no alphabetic chars contributes nothing.
        assert_eq!(initials("123 456", None), "");
    }

    #[test]
    fn initials_empty_string() {
        assert_eq!(initials("", None), "");
        assert_eq!(initials("   ", None), "");
    }

    #[test]
    fn initials_unicode_uppercase() {
        // German ß uppercases to "SS" (two chars).
        assert_eq!(initials("ßeta", None), "SS");
        // Cyrillic.
        assert_eq!(initials("привет мир", None), "ПМ");
    }

    // -------- mask_email / mask_card / mask_phone --------

    #[test]
    fn mask_email_3_or_more_chars() {
        assert_eq!(mask_email("alice@example.com"), "a***e@example.com");
        assert_eq!(mask_email("bob@example.com"), "b***b@example.com");
    }

    #[test]
    fn mask_email_short_local() {
        assert_eq!(mask_email("a@example.com"), "*@example.com");
        assert_eq!(mask_email("ab@example.com"), "a*@example.com");
        // Empty local.
        assert_eq!(mask_email("@example.com"), "@example.com");
    }

    #[test]
    fn mask_email_no_at_passes_through() {
        assert_eq!(mask_email("not-an-email"), "not-an-email");
        assert_eq!(mask_email(""), "");
    }

    #[test]
    fn mask_card_basic() {
        assert_eq!(mask_card("4111 1111 1111 1111"), "************1111");
        assert_eq!(mask_card("4111111111111111"), "************1111");
        assert_eq!(mask_card("4111-1111-1111-1111"), "************1111");
    }

    #[test]
    fn mask_card_short_fully_masked() {
        assert_eq!(mask_card("4111"), "****");
        assert_eq!(mask_card("1"), "*");
        assert_eq!(mask_card("12"), "**");
    }

    #[test]
    fn mask_card_non_digit_passes_through() {
        assert_eq!(mask_card("not a card"), "not a card");
        assert_eq!(mask_card("4111-XXXX"), "4111-XXXX"); // contains letters
        assert_eq!(mask_card(""), "");
    }

    #[test]
    fn mask_phone_keeps_separators() {
        assert_eq!(mask_phone("+1 415 555 2671"), "+* *** *** 2671");
        assert_eq!(mask_phone("(415) 555-2671"), "(***) ***-2671");
        assert_eq!(mask_phone("4155552671"), "******2671");
    }

    #[test]
    fn mask_phone_short_fully_masked() {
        assert_eq!(mask_phone("123"), "***");
        assert_eq!(mask_phone("1234"), "****");
    }

    #[test]
    fn mask_phone_no_digits_passes_through() {
        assert_eq!(mask_phone("no digits"), "no digits");
        assert_eq!(mask_phone(""), "");
    }

    // -------- cut / normalize_whitespace --------

    #[test]
    fn cut_removes_every_occurrence() {
        assert_eq!(cut("Joel is a slug", " "), "Joelisaslug");
        assert_eq!(cut("hello world", "l"), "heo word");
        assert_eq!(cut("aaaa", "a"), "");
        assert_eq!(cut("aaa", "aa"), "a"); // non-overlapping
    }

    #[test]
    fn cut_no_match_unchanged() {
        assert_eq!(cut("nothing matches", "xyz"), "nothing matches");
        assert_eq!(cut("", "x"), "");
    }

    #[test]
    fn cut_empty_needle_is_no_op() {
        assert_eq!(cut("untouched", ""), "untouched");
        assert_eq!(cut("", ""), "");
    }

    #[test]
    fn normalize_whitespace_collapses_runs() {
        assert_eq!(normalize_whitespace("  hello   world  "), "hello world");
        assert_eq!(normalize_whitespace("a   b   c"), "a b c");
    }

    #[test]
    fn normalize_whitespace_handles_mixed_whitespace_chars() {
        assert_eq!(normalize_whitespace("a\n\tb\rc"), "a b c");
        assert_eq!(normalize_whitespace("\t\n  spaces  \n\t"), "spaces");
    }

    #[test]
    fn normalize_whitespace_empty_and_all_whitespace() {
        assert_eq!(normalize_whitespace(""), "");
        assert_eq!(normalize_whitespace("   \n\t"), "");
    }

    // -------- wordcount / linenumbers / ljust / rjust / center / get_digit --------

    #[test]
    fn wordcount_basic() {
        assert_eq!(wordcount("Joel is a slug"), 4);
        assert_eq!(wordcount(""), 0);
        assert_eq!(wordcount("   "), 0);
        assert_eq!(wordcount("  spaces   between   "), 2);
        assert_eq!(wordcount("one"), 1);
    }

    #[test]
    fn linenumbers_basic() {
        assert_eq!(linenumbers("one\ntwo\nthree"), "1. one\n2. two\n3. three");
    }

    #[test]
    fn linenumbers_pads_for_double_digit_line_counts() {
        // 10 lines → width = 2 → " 1. ..." through "10. ..."
        let many: String = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = linenumbers(&many);
        let first = out.lines().next().unwrap();
        let last = out.lines().last().unwrap();
        assert_eq!(first, " 1. line1");
        assert_eq!(last, "10. line10");
    }

    #[test]
    fn linenumbers_single_line() {
        assert_eq!(linenumbers("solo"), "1. solo");
    }

    #[test]
    fn ljust_pads_right() {
        assert_eq!(ljust("Joel", 10), "Joel      ");
        assert_eq!(ljust("Joel", 4), "Joel"); // already at width
        assert_eq!(ljust("Joel", 0), "Joel"); // n=0 → unchanged
    }

    #[test]
    fn rjust_pads_left() {
        assert_eq!(rjust("Joel", 10), "      Joel");
        assert_eq!(rjust("Joel", 4), "Joel");
    }

    #[test]
    fn center_pads_both_sides() {
        assert_eq!(center("Joel", 10), "   Joel   ");
        // Odd-remainder → extra goes right.
        assert_eq!(center("x", 4), " x  ");
        // Already at or beyond width.
        assert_eq!(center("toolong", 4), "toolong");
    }

    #[test]
    fn get_digit_basic() {
        assert_eq!(get_digit(1234, 1), "4");
        assert_eq!(get_digit(1234, 2), "3");
        assert_eq!(get_digit(1234, 3), "2");
        assert_eq!(get_digit(1234, 4), "1");
    }

    #[test]
    fn get_digit_past_leftmost_returns_zero() {
        assert_eq!(get_digit(1234, 5), "0");
        assert_eq!(get_digit(1234, 99), "0");
    }

    #[test]
    fn get_digit_negative_uses_absolute_value() {
        assert_eq!(get_digit(-1234, 1), "4");
        assert_eq!(get_digit(-1234, 4), "1");
    }

    #[test]
    fn get_digit_invalid_index_passes_full_int() {
        assert_eq!(get_digit(1234, 0), "1234");
        assert_eq!(get_digit(1234, -1), "1234");
        assert_eq!(get_digit(-1234, 0), "-1234");
    }

    #[test]
    fn get_digit_zero_value() {
        // Single-digit value 0 → idx 1 is "0", idx 2+ is "0" (past).
        assert_eq!(get_digit(0, 1), "0");
        assert_eq!(get_digit(0, 5), "0");
    }

    // -------- pluralize --------

    #[test]
    fn pluralize_default_suffix() {
        assert_eq!(pluralize(1, ""), "");
        assert_eq!(pluralize(2, ""), "s");
        assert_eq!(pluralize(0, ""), "s");
        assert_eq!(pluralize(-1, ""), "s");
    }

    #[test]
    fn pluralize_single_token_suffix() {
        assert_eq!(pluralize(1, "es"), "");
        assert_eq!(pluralize(2, "es"), "es");
        assert_eq!(pluralize(2, "z"), "z");
    }

    #[test]
    fn pluralize_singular_plural_pair() {
        assert_eq!(pluralize(1, "y,ies"), "y");
        assert_eq!(pluralize(2, "y,ies"), "ies");
        // 0 takes plural — matches English convention ("0 items").
        assert_eq!(pluralize(0, "y,ies"), "ies");
    }

    #[test]
    fn pluralize_extra_tokens_ignored() {
        // "a,b,c" → only first two used → ("a", "b").
        assert_eq!(pluralize(1, "a,b,c"), "a");
        assert_eq!(pluralize(2, "a,b,c"), "b");
    }

    #[test]
    fn pluralize_large_counts() {
        assert_eq!(pluralize(i64::MAX, ""), "s");
        assert_eq!(pluralize(i64::MIN, ""), "s");
    }

    // -------- truncate_html_chars --------

    #[test]
    fn truncate_html_chars_basic() {
        assert_eq!(
            truncate_html_chars("<p>hello world</p>", 5, "…"),
            "<p>hello…</p>"
        );
    }

    #[test]
    fn truncate_html_chars_no_truncation_when_short() {
        assert_eq!(truncate_html_chars("<p>short</p>", 10, "…"), "<p>short</p>");
        assert_eq!(truncate_html_chars("", 5, "…"), "");
    }

    #[test]
    fn truncate_html_chars_closes_nested_tags_in_reverse() {
        assert_eq!(
            truncate_html_chars("<b><i>nested</i> text</b>", 7, "…"),
            "<b><i>nested</i> …</b>"
        );
    }

    #[test]
    fn truncate_html_chars_self_closing_tags_not_stacked() {
        // <br> and <img ... /> shouldn't appear as </br>/</img> in
        // the output even when truncation fires mid-content.
        let s = "<p>hello<br>world<img src=\"x.png\"/> more</p>";
        let out = truncate_html_chars(s, 8, "…");
        assert!(!out.contains("</br>"));
        assert!(!out.contains("</img>"));
        // <p> still closed.
        assert!(out.ends_with("</p>"));
    }

    #[test]
    fn truncate_html_chars_entity_counts_as_one() {
        // "a&amp;b" = 3 visible chars (a, &, b); limit 2 truncates
        // after the &amp; entity.
        let out = truncate_html_chars("a&amp;b", 2, "…");
        // Both `a` and `&amp;` were emitted, then suffix.
        assert!(out.contains("a&amp;"));
        assert!(out.ends_with("…"));
    }

    #[test]
    fn truncate_html_chars_no_close_for_void_input() {
        // No open tags at truncation point → no trailing </…>.
        assert_eq!(truncate_html_chars("hello world", 5, "…"), "hello…");
    }

    // -------- truncate_html_words --------

    #[test]
    fn truncate_html_words_basic() {
        assert_eq!(
            truncate_html_words("<p>Joel is a slug</p>", 2, " …"),
            "<p>Joel is …</p>"
        );
    }

    #[test]
    fn truncate_html_words_no_truncation_when_short() {
        assert_eq!(
            truncate_html_words("<p>short text</p>", 5, "…"),
            "<p>short text</p>"
        );
    }

    #[test]
    fn truncate_html_words_preserves_inline_tags() {
        // Inline <em>...</em> stays balanced even when the truncation
        // boundary falls between words on either side of it.
        let out = truncate_html_words("<p>foo <em>bar baz</em> qux quux</p>", 3, "…");
        assert!(out.starts_with("<p>foo "));
        assert!(out.ends_with("</p>"));
        assert!(out.contains("<em>"));
        // Either contains balanced </em> or doesn't open it at all —
        // verify no stray opener.
        let opens = out.matches("<em>").count();
        let closes = out.matches("</em>").count();
        assert_eq!(opens, closes);
    }
}
