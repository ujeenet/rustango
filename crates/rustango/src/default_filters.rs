//! Everyday template filters, registered on Tera.
//!
//! Call [`register_filters`] on a Tera instance to make them available:
//!
//! ```ignore
//! let mut tera = tera::Tera::default();
//! rustango::default_filters::register_filters(&mut tera);
//! // now {{ count | pluralize }} renders "" / "s"
//! ```
//!
//! These are the text and number filters Tera does not ship, plus a
//! few extras such as `mask_email`, `mask_card`, `oxford_join` and
//! `initials`. See [`register_filters`] for the full list.
//!
//! Output is en-US.
//!
//! [`register_filters`]: crate::default_filters::register_filters

use std::collections::HashMap;

use tera::{to_value, Tera, Value};

/// Register every filter in this module on `tera`. Call it during app
/// setup, right after `Tera::new(...)` or `Tera::default()`.
pub fn register_filters(tera: &mut Tera) {
    tera.register_filter("pluralize", pluralize);
    tera.register_filter("truncatewords", truncatewords);
    tera.register_filter("linebreaks", linebreaks);
    tera.register_filter("default_if_none", default_if_none);
    tera.register_filter("add", add);
    tera.register_filter("cut", cut);
    tera.register_filter("divisibleby", divisibleby);
    tera.register_filter("floatformat", floatformat);
    tera.register_filter("escapejs", escapejs);
    tera.register_filter("yesno", yesno);
    tera.register_filter("get_digit", get_digit);
    tera.register_filter("dictsort", dictsort);
    tera.register_filter("slugify_unicode", slugify_unicode);
    tera.register_filter("iriencode", iriencode);
    tera.register_filter("wordwrap", wordwrap);
    tera.register_filter("mask_email", mask_email);
    tera.register_filter("mask_card", mask_card);
    tera.register_filter("mask_phone", mask_phone);
    tera.register_filter("dictsortreversed", dictsortreversed);
    tera.register_filter("oxford_join", oxford_join);
    tera.register_filter("initials", initials);
    tera.register_filter("truncatechars", truncatechars);
    tera.register_filter("truncatechars_html", truncatechars_html);
    tera.register_filter("truncatewords_html", truncatewords_html);
    tera.register_filter("urlize", urlize_filter);
    tera.register_filter("avoid_wrapping", avoid_wrapping_filter);
    tera.register_filter("normalize_whitespace", normalize_whitespace);
    tera.register_filter("wordcount", wordcount);
    tera.register_filter("phone2numeric", phone2numeric);
    tera.register_filter("linenumbers", linenumbers);
    tera.register_filter("ljust", ljust);
    tera.register_filter("rjust", rjust);
    tera.register_filter("center", center_filter);
    tera.register_filter("striptags", striptags);
    tera.register_filter("capfirst", capfirst);
    tera.register_filter("addslashes", addslashes);
    tera.register_filter("filesizeformat", filesizeformat);
    tera.register_filter("length_is", length_is);
    tera.register_filter("make_list", make_list);
    tera.register_filter("pprint", pprint);
    tera.register_filter("urlizetrunc", urlizetrunc);
    tera.register_filter("unordered_list", unordered_list);
    tera.register_filter("json_script", json_script);
    tera.register_filter("is_blank", is_blank_filter);
    tera.register_filter("truncate_middle", truncate_middle_filter);
    tera.register_function("widthratio", widthratio);
}

// ------------------------------------------------------------------ pluralize

/// `pluralize` — return the singular/plural suffix that matches an
/// integer-like value:
/// - `{{ 1|pluralize }}` → `""`
/// - `{{ 2|pluralize }}` → `"s"`
/// - `{{ 1|pluralize:"es" }}` → `""`
/// - `{{ 2|pluralize:"es" }}` → `"es"`
/// - `{{ 1|pluralize:"y,ies" }}` → `"y"`
/// - `{{ 2|pluralize:"y,ies" }}` → `"ies"`
///
/// A value that is neither a number nor a collection returns `""`
/// rather than erroring, so a typo in a variable name does not break
/// the page.
fn pluralize(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let count = count_for_pluralize(value);
    let suffix_arg = args
        .get("suffix")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("s");
    Ok(to_value(crate::text::pluralize(count, suffix_arg))?)
}

/// The count that drives pluralize: an integer is
/// used directly, a float is truncated, an array, map or string uses its
/// length, and anything else is 0, so the plural form wins.
fn count_for_pluralize(value: &Value) -> i64 {
    if let Some(n) = value.as_i64() {
        return n;
    }
    if let Some(n) = value.as_u64() {
        return i64::try_from(n).unwrap_or(i64::MAX);
    }
    if let Some(f) = value.as_f64() {
        return f as i64;
    }
    if let Some(s) = value.as_str() {
        return i64::try_from(s.chars().count()).unwrap_or(i64::MAX);
    }
    if let Some(arr) = value.as_array() {
        return i64::try_from(arr.len()).unwrap_or(i64::MAX);
    }
    if let Some(obj) = value.as_object() {
        return i64::try_from(obj.len()).unwrap_or(i64::MAX);
    }
    0
}

// ------------------------------------------------------------------ truncatewords

/// `truncatewords` — keep the first N words, append `…` if any
/// were dropped:
/// - `{{ "Joel is a slug"|truncatewords:2 }}` → `"Joel is …"`
/// - `{{ "two words"|truncatewords:5 }}` → `"two words"`
///
/// A zero, negative or non-integer argument gives an empty string.
/// Runs of whitespace collapse to a single space in the output.
fn truncatewords(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        // Non-string values pass through rather than erroring.
        return Ok(value.clone());
    };
    let n = args
        .get("count")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if n <= 0 {
        // Not `text::truncate_words(s, 0, …)`, which emits a stray
        // suffix. An empty string is the right answer here.
        return Ok(to_value("")?);
    }
    let n = usize::try_from(n).unwrap_or(0);
    Ok(to_value(crate::text::truncate_words(s, n, " …"))?)
}

// ------------------------------------------------------------------ linebreaks

/// `linebreaks` — turn plain-text line breaks into HTML. A blank line
/// starts a new `<p>`; a single newline becomes `<br>`. The input is
/// HTML-escaped first, so raw `<script>` cannot leak through.
///
/// - `"foo\nbar"` → `"<p>foo<br>bar</p>"`
/// - `"foo\n\nbar"` → `"<p>foo</p>\n\n<p>bar</p>"`
/// - `""` → `""` (empty input passes through, no empty `<p>`).
fn linebreaks(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    if s.is_empty() {
        return Ok(to_value("")?);
    }
    Ok(to_value(crate::text::linebreaks(s, true))?)
}

// ------------------------------------------------------------------ default_if_none

/// `default_if_none` — replace `null` with the argument. Tera's built-in
/// `default` replaces *undefined* values instead.
/// - `{{ user.bio|default_if_none:"(no bio)" }}` → `"(no bio)"` when
///   `bio` is JSON null
/// - `{{ "hello"|default_if_none:"x" }}` → `"hello"`
/// - An empty string is not null, so it passes through.
fn default_if_none(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    if value.is_null() {
        let fallback = args
            .get("default")
            .or_else(|| args.values().next())
            .cloned()
            .unwrap_or(Value::String(String::new()));
        return Ok(fallback);
    }
    Ok(value.clone())
}

// ------------------------------------------------------------------ add

/// `add` — a universal addition filter. Numbers add, strings
/// join, arrays concatenate, and anything else falls back to joining
/// the two stringified values.
/// - `{{ 4|add:5 }}` → `"9"`
/// - `{{ "abc"|add:"def" }}` → `"abcdef"`
/// - `{{ [1, 2]|add:[3, 4] }}` → `"[1, 2, 3, 4]"`
fn add(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let rhs = args.get("value").or_else(|| args.values().next());
    let Some(rhs) = rhs else {
        return Ok(value.clone());
    };
    // Numeric path: both sides read as numbers.
    if let (Some(a), Some(b)) = (value.as_i64(), rhs.as_i64()) {
        return Ok(to_value(a + b)?);
    }
    if let (Some(a), Some(b)) = (value.as_f64(), rhs.as_f64()) {
        return Ok(to_value(a + b)?);
    }
    // Both sides are arrays: concatenate.
    if let (Some(a), Some(b)) = (value.as_array(), rhs.as_array()) {
        let mut out = a.clone();
        out.extend(b.iter().cloned());
        return Ok(Value::Array(out));
    }
    // Strings, or mixed types: join the stringified values.
    let lhs_s = value_to_string(value);
    let rhs_s = value_to_string(rhs);
    Ok(to_value(format!("{lhs_s}{rhs_s}"))?)
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ------------------------------------------------------------------ cut

/// `cut` — remove every occurrence of the argument from the value.
/// - `{{ "Hello, world"|cut:"l" }}` → `"Heo, word"`
/// - `{{ "abc abc"|cut:"abc" }}` → `" "` (one space remains)
///
/// An empty argument returns the value unchanged.
fn cut(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let needle = args
        .get("needle")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("");
    Ok(to_value(crate::text::cut(s, needle))?)
}

// ------------------------------------------------------------------ divisibleby

/// `divisibleby` — `true` when the value divides evenly by the argument.
/// A non-integer value or a zero divisor gives `false`. Mostly used in
/// `{% if %}`: `{% if forloop.counter|divisibleby:3 %}new row{% endif %}`.
fn divisibleby(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(n) => n,
        None => match value.as_f64() {
            Some(f) => f as i64,
            None => return Ok(Value::Bool(false)),
        },
    };
    let divisor = args
        .get("divisor")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if divisor == 0 {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(n % divisor == 0))
}

// ------------------------------------------------------------------ floatformat

/// `floatformat` — a float formatter, not a plain `round`:
/// - `{{ 34.23234|floatformat }}` → `"34.2"` (one decimal by default)
/// - `{{ 34.00000|floatformat }}` → `"34"` (a zero decimal is dropped)
/// - `{{ 34.23234|floatformat:3 }}` → `"34.232"`
/// - `{{ 34.00000|floatformat:3 }}` → `"34.000"` (positive keeps zeros)
/// - `{{ 34.0|floatformat:-3 }}` → `"34"` (negative drops zeros)
///
/// A negative precision means "at most N decimals, hidden when the
/// value is round". Good for prices, where `$5.00` should read `$5`.
fn floatformat(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(f) = value.as_f64() else {
        return Ok(value.clone());
    };
    let precision: i64 = args
        .get("precision")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    Ok(to_value(crate::numberformat::floatformat(f, precision))?)
}

// ------------------------------------------------------------------ escapejs

/// `escapejs` — escape a string so it is safe inside a JS string
/// literal in HTML:
///
/// ```html
/// <script>var s = "{{ value|escapejs }}";</script>
/// ```
///
/// Every character that means something to HTML or to JS becomes a
/// `\uXXXX` escape: quotes, slashes, brackets, `&`, `=`, `-`, `;`,
/// backticks, U+2028 / U+2029 and all control characters. So no input
/// can inject `</script>` or break the JS syntax.
fn escapejs(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::escapejs(s))?)
}

// ------------------------------------------------------------------ yesno

/// `yesno` — map a boolean, or null, onto one of up to three strings.
/// - `{{ true|yesno:"yes,no" }}` → `"yes"`
/// - `{{ false|yesno:"yes,no" }}` → `"no"`
/// - `{{ null|yesno:"yes,no,maybe" }}` → `"maybe"`
/// - `{{ null|yesno:"yes,no" }}` → `"no"` (no third token)
///
/// The argument is comma-separated and defaults to `"yes,no,maybe"`.
fn yesno(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let raw = args
        .get("choices")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("yes,no,maybe");
    // null → None, bool → Some(b), anything else → Some(true).
    let opt = if value.is_null() {
        None
    } else {
        Some(value.as_bool().unwrap_or(true))
    };
    Ok(to_value(crate::text::yesno(opt, raw))?)
}

// ------------------------------------------------------------------ get_digit

/// `get_digit` — the Nth digit of an integer, counted from the right
/// starting at 1.
/// - `{{ 1234|get_digit:1 }}` → `"4"`
/// - `{{ 1234|get_digit:4 }}` → `"1"`
/// - `{{ 1234|get_digit:5 }}` → `"0"` (past the leftmost digit)
///
/// A non-integer value, or an index below 1, passes through unchanged.
fn get_digit(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(n) = value.as_i64() else {
        return Ok(value.clone());
    };
    let idx = args
        .get("index")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if idx < 1 {
        return Ok(value.clone());
    }
    Ok(to_value(crate::text::get_digit(n, idx))?)
}

// ------------------------------------------------------------------ dictsort

/// `dictsort` — stable sort of a list of objects by a named key, for
/// example `{{ users|dictsort:"name" }}`.
///
/// Ordering follows [`compare_values`]. Entries missing the key count as
/// `null` and sort first. Non-list input passes through unchanged.
///
/// Dotted key paths such as `"address.city"` are not supported.
fn dictsort(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(arr) = value.as_array() else {
        return Ok(value.clone());
    };
    let key = args
        .get("key")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("");
    if key.is_empty() {
        return Ok(value.clone());
    }
    let mut sorted = arr.clone();
    sorted.sort_by(|a, b| {
        let ak = a.get(key).cloned().unwrap_or(Value::Null);
        let bk = b.get(key).cloned().unwrap_or(Value::Null);
        compare_values(&ak, &bk)
    });
    Ok(Value::Array(sorted))
}

/// `dictsortreversed` — [`dictsort`] in descending order. Missing-key
/// entries count as `null`, so they sort last here.
fn dictsortreversed(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(arr) = value.as_array() else {
        return Ok(value.clone());
    };
    let key = args
        .get("key")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("");
    if key.is_empty() {
        return Ok(value.clone());
    }
    let mut sorted = arr.clone();
    sorted.sort_by(|a, b| {
        let ak = a.get(key).cloned().unwrap_or(Value::Null);
        let bk = b.get(key).cloned().unwrap_or(Value::Null);
        compare_values(&bk, &ak) // swap for reversed
    });
    Ok(Value::Array(sorted))
}

// ------------------------------------------------------------------ oxford_join

/// `oxford_join` — join a list into a natural-language list with the
/// Oxford comma. The conjunction defaults to `"and"`:
///
/// - `["a"]` → `"a"`
/// - `["a", "b"]` → `"a and b"` (no comma)
/// - `["a", "b", "c"]` → `"a, b, and c"`
///
/// Pass `conj` to change it:
///
/// ```jinja
/// {{ items | oxford_join(conj="or") }}  {# "a, b, or c" #}
/// ```
///
/// Non-string elements are stringified. Non-array input passes through.
fn oxford_join(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(arr) = value.as_array() else {
        return Ok(value.clone());
    };
    let conj = args
        .get("conj")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("and");
    let items: Vec<String> = arr
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    Ok(to_value(crate::text::oxford_join(&items, conj))?)
}

// ------------------------------------------------------------------ initials

/// `initials` — the uppercase first letter of each word, for the avatar
/// fallback shown when a user has no picture. `count` caps how many you
/// get. Leading non-letters are skipped, so `"123 Alice"` gives `"A"`.
///
/// - `"Alice Bob"` → `"AB"`
/// - `"alice m. bob"` → `"AMB"`
/// - `"alice m. bob" | initials(count=2)` → `"AM"`
fn initials(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let limit = args
        .get("count")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    Ok(to_value(crate::text::initials(s, limit))?)
}

// ------------------------------------------------------------------ truncatechars

/// `truncatechars` — cut the input to at most `count` characters and
/// append `…` if anything was dropped. The `…` counts toward `count`, so
/// the output is never longer than `count`. Tera's built-in `truncate`
/// differs: it adds a literal `...` on top of the count.
///
/// - `"Joel is a slug" | truncatechars(count=7)` → `"Joel i…"`
/// - `"Hi" | truncatechars(count=10)` → `"Hi"`
/// - `"abcd" | truncatechars(count=3)` → `"ab…"`
/// - `"any" | truncatechars(count=0)` → `""`
///
/// A negative or non-integer `count` returns the input unchanged.
fn truncatechars(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let n = match args
        .get("count")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
    {
        Some(n) if n >= 0 => n as usize,
        _ => return Ok(value.clone()),
    };
    Ok(to_value(crate::text::truncatechars(s, n))?)
}

// ------------------------------------------------------------------ truncatechars_html / truncatewords_html

/// `truncatechars_html` — [`truncatechars`] that understands tags. It
/// counts only visible characters and tracks open tags, so the result is
/// still well-formed HTML. Void elements such as `<br>` are not stacked.
/// As with [`truncatechars`], the `…` counts toward `count`.
///
/// ```jinja
/// {{ "<p>hello world</p>" | truncatechars_html(count=7) }}
/// {# → "<p>hello …</p>" — 6 visible chars + ellipsis = 7 total #}
/// ```
///
/// A negative or non-integer `count` returns the input unchanged;
/// `count = 0` returns an empty string.
fn truncatechars_html(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let n = match args
        .get("count")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
    {
        Some(n) if n >= 0 => n as usize,
        _ => return Ok(value.clone()),
    };
    // Reserve one visible char for the ellipsis.
    let visible_budget = n.saturating_sub(1);
    Ok(to_value(crate::text::truncate_html_chars(
        s,
        visible_budget,
        "…",
    ))?)
}

/// `truncatewords_html` — `truncatewords` that understands tags. It
/// counts only words outside tags and closes any open tags at the end.
///
/// ```jinja
/// {{ "<p>Joel is a slug</p>" | truncatewords_html(count=2) }}
/// {# → "<p>Joel is …</p>" #}
/// ```
fn truncatewords_html(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let n = match args
        .get("count")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
    {
        Some(n) if n >= 0 => n as usize,
        _ => return Ok(value.clone()),
    };
    Ok(to_value(crate::text::truncate_html_words(s, n, " …"))?)
}

// ------------------------------------------------------------------ urlize

/// `urlize` — turn `http(s)://…`, `www.host.tld/…` and `user@host.tld`
/// into `<a>` elements. Pass `nofollow=true` to add `rel="nofollow"`;
/// it is off by default.
///
/// ```jinja
/// {{ "see http://example.com" | urlize | safe }}
/// {# → "see <a href=\"http://example.com\">http://example.com</a>" #}
/// {{ comment | urlize(nofollow=true) | safe }}
/// ```
fn urlize_filter(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let nofollow = args
        .get("nofollow")
        .or_else(|| args.values().next())
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(to_value(crate::text::urlize(s, nofollow))?)
}

// ------------------------------------------------------------------ avoid_wrapping

/// `avoid_wrapping` — replace spaces with non-breaking spaces so the
/// phrase stays on one line. Good for dates, version strings and brand
/// names that look wrong when broken across lines.
///
/// ```jinja
/// {{ "June 5" | avoid_wrapping }}   {# → "June\u{a0}5" #}
/// ```
fn avoid_wrapping_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::avoid_wrapping(s))?)
}

// ------------------------------------------------------------------ normalize_whitespace

/// `normalize_whitespace` — collapse every run of whitespace into one
/// space and trim the ends. Use it on free text shown on a single line,
/// such as an admin column, an email subject or a page title.
///
/// - `"  hello   world  "` → `"hello world"`
/// - `"line\n\n\twith tabs"` → `"line with tabs"`
fn normalize_whitespace(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::normalize_whitespace(s))?)
}

/// Total order over JSON values: null < bool < number < string < array
/// < object. Two values of the same type use that type's own order.
fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Number(_) => 2,
            Value::String(_) => 3,
            Value::Array(_) => 4,
            Value::Object(_) => 5,
        }
    }
    let ra = rank(a);
    let rb = rank(b);
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&y.as_f64().unwrap_or(0.0))
            .unwrap_or(Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        // Arrays and objects compare by their JSON text, so the sort
        // stays deterministic.
        _ => a.to_string().cmp(&b.to_string()),
    }
}

// ------------------------------------------------------------------ slugify_unicode

/// `slugify_unicode` — makes a URL-safe slug but keeps non-ASCII
/// letters, so it works for users who
/// write in a non-Latin script.
///
/// It lowercases everything, keeps Unicode letters, digits and `_`,
/// turns every run of other characters into one `-`, and trims `-` from
/// both ends.
///
/// ```jinja
/// {{ "Hello World!" | slugify_unicode }}   {# → "hello-world" #}
/// {{ "Привет мир"   | slugify_unicode }}   {# → "привет-мир" #}
/// {{ "café-au-lait" | slugify_unicode }}   {# → "café-au-lait" #}
/// ```
///
/// Tera's own `slugify` is ASCII-only: it transliterates or drops
/// non-ASCII. Use this one when you want Unicode in URLs.
fn slugify_unicode(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_alphanumeric() || ch == '_' {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !out.is_empty() {
            out.push('-');
            last_was_dash = true;
        }
        // Otherwise skip: collapse the run, and never lead with a dash.
    }
    while out.ends_with('-') {
        out.pop();
    }
    Ok(to_value(out)?)
}

// ------------------------------------------------------------------ iriencode

/// `iriencode` — encode an [IRI](https://tools.ietf.org/html/rfc3987)
/// for use as a URL. Only the bytes a URI cannot hold are
/// percent-encoded; `/`, `:`, `?`, `#`, `=`, `&` and friends stay as
/// they are, so the URL structure survives:
///
/// ```jinja
/// <a href="{{ url | iriencode }}">link</a>
/// ```
///
/// Use it for `href` and `src`. Tera's `urlencode` is for a single
/// query-string value: it encodes everything but `[a-zA-Z0-9_-]`, which
/// would break a whole URL.
fn iriencode(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::url_codec::iri_to_uri(s))?)
}

// ------------------------------------------------------------------ wordwrap

/// `wordwrap` — wrap text at word boundaries so no line is longer than
/// `width`. Useful for plain-text email, SMS and fixed-width displays.
///
/// - `{{ "Joel is a slug"|wordwrap:5 }}` → `"Joel\nis a\nslug"`
/// - `{{ "one two three"|wordwrap:7 }}` → `"one two\nthree"`
///
/// Existing newlines are kept, so already-wrapped text is not reflowed.
/// A word longer than `width` gets its own line rather than a hyphen.
/// `width <= 0` returns the input unchanged.
fn wordwrap(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let width = args
        .get("width")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if width <= 0 {
        return Ok(value.clone());
    }
    let width = usize::try_from(width).unwrap_or(usize::MAX);
    Ok(to_value(crate::text::wrap(s, width))?)
}

// ------------------------------------------------------------------ mask_email

/// `mask_email` — hide most of an email address, for admin lists and
/// audit logs where the full address would be over-sharing.
///
/// The first and last character of the local part stay and the middle
/// becomes `***`. The domain is untouched. A string with no `@` passes
/// through, and this does not check that the input is a valid address —
/// use [`crate::validators::validate_email`] for that.
///
/// - `alice@example.com` → `a***e@example.com`
/// - `a@example.com` → `*@example.com`
/// - `not-an-email` → `not-an-email`
fn mask_email(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    if !s.contains('@') {
        return Ok(value.clone());
    }
    Ok(to_value(crate::text::mask_email(s))?)
}

// ------------------------------------------------------------------ mask_card

/// `mask_card` — mask a card number as `************1234`. Spaces and
/// hyphens are stripped first, then every digit but the last four
/// becomes `*`. Four digits or fewer are masked completely, and input
/// with no digits passes through.
///
/// - `"4111 1111 1111 1111"` → `"************1111"`
/// - `"4111"` → `"****"`
/// - `"not a card"` → `"not a card"`
///
/// Validate at intake with
/// [`crate::validators::validate_creditcard_luhn`]; use this at render
/// time, for example on a "card on file" line.
fn mask_card(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::mask_card(s))?)
}

// ------------------------------------------------------------------ mask_phone

/// `mask_phone` — mask every digit of a phone number but the last four,
/// keeping the separators. Use it in admin lists and order summaries.
///
/// - `"+1 415 555 2671"` → `"+* *** *** 2671"`
/// - `"(415) 555-2671"` → `"(***) ***-2671"`
/// - `"123"` → `"***"` (four digits or fewer are fully masked)
/// - `"no digits"` → `"no digits"`
fn mask_phone(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::mask_phone(s))?)
}

// ------------------------------------------------------------------ wordcount

/// `wordcount` — number of whitespace-separated words.
/// `{{ "Joel is a slug"|wordcount }}` → `4`.
fn wordcount(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    Ok(to_value(crate::text::wordcount(s))?)
}

// ------------------------------------------------------------------ phone2numeric

/// `phone2numeric` — turn phone-keypad letters into digits, using the
/// standard ITU E.161 mapping (`abc → 2` … `wxyz → 9`). Case does not
/// matter, and non-letters pass through.
/// `{{ "1-800-COLLECT"|phone2numeric }}` → `"1-800-2655328"`.
fn phone2numeric(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::phone2numeric(s))?)
}

// ------------------------------------------------------------------ linenumbers

/// `linenumbers` — put a 1-based line number in front of every line,
/// padded to the width of the largest number. So `"one\ntwo\nthree"`
/// becomes `"1. one\n2. two\n3. three"`, while 100 lines run from
/// `"  1. …"` to `"100. …"`.
fn linenumbers(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::linenumbers(s))?)
}

// ------------------------------------------------------------------ ljust / rjust / center

fn pad_arg(args: &HashMap<String, Value>) -> usize {
    args.get("width")
        .or_else(|| args.values().next())
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

/// `ljust:N` — pad on the right with spaces to width N.
/// `{{ "Joel"|ljust:10 }}` → `"Joel      "`. A value already N or
/// longer passes through unchanged.
fn ljust(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let width = pad_arg(args);
    Ok(to_value(crate::text::ljust(s, width))?)
}

/// `rjust:N` — pad on the left with spaces to width N.
/// `{{ "Joel"|rjust:10 }}` → `"      Joel"`.
fn rjust(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let width = pad_arg(args);
    Ok(to_value(crate::text::rjust(s, width))?)
}

/// `center:N` — center the value in a field of width N.
/// `{{ "Joel"|center:10 }}` → `"   Joel   "`. When the padding does not
/// split evenly, the extra space goes on the right.
fn center_filter(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let width = pad_arg(args);
    Ok(to_value(crate::text::center(s, width))?)
}

/// `striptags` — remove HTML tags and return the text. **This is not a
/// sanitizer.** Use it only on input you already trust.
fn striptags(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    Ok(to_value(crate::text::strip_tags(s))?)
}

/// `capfirst` — uppercase the first character and leave the rest alone.
fn capfirst(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    Ok(to_value(crate::text::capfirst(s))?)
}

/// `addslashes` — put a backslash before every quote and backslash.
/// Useful for CSV or JS literals, where Tera's autoescape does not
/// apply.
fn addslashes(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    Ok(to_value(out)?)
}

/// `length_is:"N"` — `true` when the length equals `N`. A string counts
/// characters, not bytes; an array or object counts its entries.
fn length_is(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let target = args
        .get("arg")
        .and_then(|v| match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        })
        .unwrap_or(-1);
    if target < 0 {
        return Ok(to_value(false)?);
    }
    let actual = match value {
        Value::String(s) => s.chars().count() as i64,
        Value::Array(a) => a.len() as i64,
        Value::Object(o) => o.len() as i64,
        Value::Null => 0,
        _ => -1,
    };
    Ok(to_value(actual == target)?)
}

/// `make_list` — split a string into its characters, or a number into
/// its digits, for `{% for char in value|make_list %}`.
fn make_list(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let chars: Vec<String> = match value {
        Value::String(s) => s.chars().map(|c| c.to_string()).collect(),
        Value::Number(n) => n.to_string().chars().map(|c| c.to_string()).collect(),
        Value::Bool(b) => b.to_string().chars().map(|c| c.to_string()).collect(),
        _ => return Ok(value.clone()),
    };
    Ok(to_value(chars)?)
}

/// `pprint` — pretty-print the value for debugging. The output is
/// indented JSON, not Python's `repr`.
fn pprint(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    Ok(to_value(s)?)
}

/// `urlizetrunc:"N"` — like `urlize`, but the visible link text is cut
/// to at most `N` characters, with `...` appended. The `href` itself
/// does not change.
fn urlizetrunc(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let limit = args
        .get("arg")
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64().map(|n| n as usize),
            Value::String(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(usize::MAX);
    let urlized = crate::text::urlize(s, false);
    let out = truncate_link_text(&urlized, limit);
    Ok(to_value(out)?)
}

/// Cut the visible text of each `<a>` in a urlized string to `limit`
/// characters, appending `...`. The `href` is left alone.
fn truncate_link_text(html: &str, limit: usize) -> String {
    if limit == usize::MAX {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(close) = html[i..].find('>') {
                let tag = &html[i..i + close + 1];
                out.push_str(tag);
                i += close + 1;
                if tag.starts_with("<a ") || tag == "<a>" {
                    // Take the anchor text up to </a>.
                    if let Some(end) = html[i..].find("</a>") {
                        let anchor_text = &html[i..i + end];
                        if anchor_text.chars().count() > limit {
                            let truncated: String =
                                anchor_text.chars().take(limit.saturating_sub(3)).collect();
                            out.push_str(&truncated);
                            out.push_str("...");
                        } else {
                            out.push_str(anchor_text);
                        }
                        out.push_str("</a>");
                        i += end + 4;
                    }
                }
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// `widthratio` — compute `round((value / max) * width)`, for bar
/// charts and progress bars. A Tera function with keyword
/// arguments:
///
/// ```jinja
/// <div style="width: {{ widthratio(value=this, max=upper, width=200) }}px"></div>
/// ```
///
/// Each argument may be an integer or a float. The result is 0 when
/// `max` is 0 or an argument is not a number, so a zero divisor never
/// panics.
fn widthratio(args: &HashMap<String, Value>) -> tera::Result<Value> {
    let as_f = |k: &str| -> Option<f64> {
        args.get(k).and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        })
    };
    let v = match as_f("value") {
        Some(v) => v,
        None => return Ok(to_value(0_i64)?),
    };
    let m = match as_f("max") {
        Some(m) if m != 0.0 => m,
        _ => return Ok(to_value(0_i64)?),
    };
    let w = match as_f("width") {
        Some(w) => w,
        None => return Ok(to_value(0_i64)?),
    };
    let result = ((v / m) * w).round() as i64;
    Ok(to_value(result)?)
}

/// `is_blank` — `true` for null, an empty string, or a string of only
/// whitespace. Everything else is `false`.
/// Typical use: `{% if user.bio|is_blank %}(no bio yet){% endif %}`.
fn is_blank_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let blank = match value {
        Value::Null => true,
        Value::String(s) => crate::text::is_blank(s),
        _ => false,
    };
    Ok(to_value(blank)?)
}

/// `truncate_middle` — keep the head and tail and put a placeholder in
/// the middle, for showing a SHA, UUID or path in a narrow cell.
/// `width` defaults to 32 and `placeholder` to `"…"`:
/// `{{ s|truncate_middle(width=10) }}`.
fn truncate_middle_filter(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = match value {
        Value::String(s) => s.as_str(),
        _ => return Ok(value.clone()),
    };
    let width = args
        .get("width")
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64().map(|n| n as usize),
            Value::String(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(32);
    let placeholder = args
        .get("placeholder")
        .and_then(|v| v.as_str())
        .unwrap_or("…");
    Ok(to_value(crate::text::truncate_middle(
        s,
        width,
        placeholder,
    ))?)
}

/// `json_script:"id"` — render the value inside a
/// `<script id="…" type="application/json">` block, escaped so it is
/// safe in HTML. The element id defaults to `"id"`; pass your own.
fn json_script(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let element_id = args
        .get("arg")
        .or_else(|| args.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("id");
    let rendered = crate::text::json_script(value, element_id)
        .map_err(|e| tera::Error::msg(format!("json_script: {e}")))?;
    Ok(to_value(rendered)?)
}

/// `unordered_list` — render a nested array as a `<ul>` tree. The
/// input is a list of lists, where a label may be followed
/// by a list of its children:
/// `["States", ["Kansas", ["Lawrence", "Topeka"], "Illinois"]]`.
///
/// Text nodes are escaped. Add `|safe` afterwards if the content is
/// already trusted HTML.
fn unordered_list(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let arr = match value {
        Value::Array(a) => a,
        _ => return Ok(value.clone()),
    };
    let mut out = String::new();
    render_unordered_list(arr, &mut out);
    Ok(to_value(out)?)
}

fn render_unordered_list(items: &[Value], out: &mut String) {
    let mut i = 0;
    while i < items.len() {
        out.push_str("<li>");
        match &items[i] {
            Value::String(s) => out.push_str(&crate::text::html_escape(s)),
            other => out.push_str(&crate::text::html_escape(&other.to_string())),
        }
        if let Some(Value::Array(children)) = items.get(i + 1) {
            out.push_str("\n<ul>");
            render_unordered_list(children, out);
            out.push_str("</ul>\n");
            i += 2;
        } else {
            i += 1;
        }
        out.push_str("</li>");
    }
}

/// `filesizeformat` — a byte count as "13 KB" or "4.1 MB". An alias
/// of `humanize::naturalsize`.
fn filesizeformat(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    Ok(to_value(crate::humanize::naturalsize(n))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------- pluralize --------

    fn args_pos(v: Value) -> HashMap<String, Value> {
        // Tera passes positional filter args via the "0" key (or any
        // key — a `:arg` becomes a single named arg in Tera's
        // shape). Both registration paths put the arg through, so
        // we just stuff it under "0" / "suffix" — `pluralize` looks
        // at both.
        let mut m = HashMap::new();
        m.insert("0".to_owned(), v);
        m
    }

    #[test]
    fn pluralize_no_arg_one_yields_empty() {
        let out = pluralize(&json!(1), &HashMap::new()).unwrap();
        assert_eq!(out, json!(""));
    }

    #[test]
    fn pluralize_no_arg_zero_yields_s() {
        let out = pluralize(&json!(0), &HashMap::new()).unwrap();
        assert_eq!(out, json!("s"));
    }

    #[test]
    fn pluralize_no_arg_two_yields_s() {
        let out = pluralize(&json!(2), &HashMap::new()).unwrap();
        assert_eq!(out, json!("s"));
    }

    #[test]
    fn pluralize_with_single_token_arg_uses_it_as_plural() {
        let out = pluralize(&json!(2), &args_pos(json!("es"))).unwrap();
        assert_eq!(out, json!("es"));
        let out_one = pluralize(&json!(1), &args_pos(json!("es"))).unwrap();
        assert_eq!(out_one, json!(""));
    }

    #[test]
    fn pluralize_with_singular_and_plural_tokens() {
        let two = pluralize(&json!(2), &args_pos(json!("y,ies"))).unwrap();
        assert_eq!(two, json!("ies"));
        let one = pluralize(&json!(1), &args_pos(json!("y,ies"))).unwrap();
        assert_eq!(one, json!("y"));
    }

    #[test]
    fn pluralize_uses_array_length() {
        // Passing a list runs pluralize against its length.
        let one = pluralize(&json!(["a"]), &HashMap::new()).unwrap();
        assert_eq!(one, json!(""));
        let three = pluralize(&json!(["a", "b", "c"]), &HashMap::new()).unwrap();
        assert_eq!(three, json!("s"));
    }

    // -------- truncatewords --------

    #[test]
    fn truncatewords_keeps_full_input_when_under_limit() {
        let out = truncatewords(&json!("two words"), &args_pos(json!(5))).unwrap();
        assert_eq!(out, json!("two words"));
    }

    #[test]
    fn truncatewords_trims_to_n_and_appends_ellipsis() {
        let out = truncatewords(&json!("Joel is a slug"), &args_pos(json!(2))).unwrap();
        assert_eq!(out, json!("Joel is …"));
    }

    #[test]
    fn truncatewords_collapses_multi_whitespace() {
        // Whitespace is normalized on join. Input has tabs +
        // multiple spaces; output is single-spaced.
        let out = truncatewords(&json!("a\tb   c"), &args_pos(json!(2))).unwrap();
        assert_eq!(out, json!("a b …"));
    }

    #[test]
    fn truncatewords_zero_or_negative_returns_empty() {
        let zero = truncatewords(&json!("anything"), &args_pos(json!(0))).unwrap();
        assert_eq!(zero, json!(""));
        let neg = truncatewords(&json!("anything"), &args_pos(json!(-1))).unwrap();
        assert_eq!(neg, json!(""));
    }

    #[test]
    fn truncatewords_passes_non_string_through() {
        let out = truncatewords(&json!(42), &args_pos(json!(2))).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- linebreaks --------

    #[test]
    fn linebreaks_wraps_single_paragraph_in_p_with_br_for_newlines() {
        let out = linebreaks(&json!("foo\nbar"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("<p>foo<br>bar</p>"));
    }

    #[test]
    fn linebreaks_splits_blank_lines_into_separate_paragraphs() {
        let out = linebreaks(&json!("foo\n\nbar"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("<p>foo</p>\n\n<p>bar</p>"));
    }

    #[test]
    fn linebreaks_html_escapes_input() {
        let out = linebreaks(&json!("<script>x</script>"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("<p>&lt;script&gt;x&lt;/script&gt;</p>"));
    }

    #[test]
    fn linebreaks_empty_input_passes_through() {
        let out = linebreaks(&json!(""), &HashMap::new()).unwrap();
        assert_eq!(out, json!(""));
    }

    #[test]
    fn linebreaks_normalizes_crlf_line_endings() {
        // Windows-style \r\n must still split paragraphs the same way.
        let out = linebreaks(&json!("foo\r\nbar\r\n\r\nbaz"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("<p>foo<br>bar</p>\n\n<p>baz</p>"));
    }

    // -------- default_if_none --------

    #[test]
    fn default_if_none_replaces_null() {
        let out = default_if_none(&Value::Null, &args_pos(json!("fallback"))).unwrap();
        assert_eq!(out, json!("fallback"));
    }

    #[test]
    fn default_if_none_passes_non_null_through() {
        let out = default_if_none(&json!("hi"), &args_pos(json!("fallback"))).unwrap();
        assert_eq!(out, json!("hi"));
    }

    #[test]
    fn default_if_none_empty_string_is_not_null() {
        // Empty string is a real value — it passes through. Only
        // null is treated as missing.
        let out = default_if_none(&json!(""), &args_pos(json!("fallback"))).unwrap();
        assert_eq!(out, json!(""));
    }

    #[test]
    fn default_if_none_passes_zero_through() {
        // 0 is not null. The whole point of `_if_none` vs `default`.
        let out = default_if_none(&json!(0), &args_pos(json!("fallback"))).unwrap();
        assert_eq!(out, json!(0));
    }

    // -------- register_filters --------

    #[test]
    fn register_filters_makes_pluralize_callable_via_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ n|pluralize }}").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &2);
        assert_eq!(tera.render("t", &ctx).unwrap(), "s");
    }

    #[test]
    fn register_filters_makes_truncatewords_callable_via_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s|truncatewords(count=2) }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "the quick brown fox");
        assert_eq!(tera.render("t", &ctx).unwrap(), "the quick …");
    }

    #[test]
    fn register_filters_makes_linebreaks_callable_via_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s|linebreaks|safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "a\nb");
        assert_eq!(tera.render("t", &ctx).unwrap(), "<p>a<br>b</p>");
    }

    // -------- truncatechars_html / truncatewords_html --------

    #[test]
    fn truncatechars_html_preserves_tag_structure() {
        // count=7 → 6 visible chars kept ("hello ") + ellipsis = 7
        // visible total. Tag structure preserved.
        let out = truncatechars_html(&json!("<p>hello world</p>"), &args_pos(json!(7))).unwrap();
        assert_eq!(out, json!("<p>hello …</p>"));
    }

    #[test]
    fn truncatechars_html_no_truncation_short_input() {
        let out = truncatechars_html(&json!("<p>short</p>"), &args_pos(json!(50))).unwrap();
        assert_eq!(out, json!("<p>short</p>"));
    }

    #[test]
    fn truncatechars_html_invalid_count_passes_through() {
        // Negative count → passthrough.
        let out = truncatechars_html(&json!("<p>x</p>"), &args_pos(json!(-1))).unwrap();
        assert_eq!(out, json!("<p>x</p>"));
    }

    #[test]
    fn truncatewords_html_preserves_tag_structure() {
        let out = truncatewords_html(&json!("<p>Joel is a slug</p>"), &args_pos(json!(2))).unwrap();
        assert_eq!(out, json!("<p>Joel is …</p>"));
    }

    #[test]
    fn truncatewords_html_no_truncation_short_input() {
        let out = truncatewords_html(&json!("<p>short text</p>"), &args_pos(json!(10))).unwrap();
        assert_eq!(out, json!("<p>short text</p>"));
    }

    #[test]
    fn register_filters_makes_truncatechars_html_callable_via_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s | truncatechars_html(count=7) | safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "<p>hello world</p>");
        assert_eq!(tera.render("t", &ctx).unwrap(), "<p>hello …</p>");
    }

    #[test]
    fn register_filters_makes_truncatewords_html_callable_via_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s | truncatewords_html(count=2) | safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "<p>Joel is a slug</p>");
        assert_eq!(tera.render("t", &ctx).unwrap(), "<p>Joel is …</p>");
    }

    // -------- urlize / avoid_wrapping --------

    #[test]
    fn urlize_filter_wraps_http_url_in_anchor() {
        let out = urlize_filter(&json!("see http://example.com"), &HashMap::new()).unwrap();
        let out = out.as_str().unwrap();
        assert!(out.contains(r#"<a href="http://example.com""#));
        assert!(out.contains(r#">http://example.com</a>"#));
    }

    #[test]
    fn urlize_filter_nofollow_arg_adds_rel() {
        let out = urlize_filter(&json!("http://x.com"), &args_pos(json!(true))).unwrap();
        assert!(out.as_str().unwrap().contains(r#"rel="nofollow""#));
        // Default (no arg) → no rel.
        let out_plain = urlize_filter(&json!("http://x.com"), &HashMap::new()).unwrap();
        assert!(!out_plain.as_str().unwrap().contains("nofollow"));
    }

    #[test]
    fn urlize_filter_non_string_passes_through() {
        let out = urlize_filter(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    #[test]
    fn register_filters_makes_urlize_callable_via_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s | urlize | safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "see http://example.com");
        let out = tera.render("t", &ctx).unwrap();
        assert!(out.contains(r#"<a href="http://example.com""#));
    }

    #[test]
    fn avoid_wrapping_filter_swaps_spaces() {
        let out = avoid_wrapping_filter(&json!("June 5"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("June\u{00A0}5"));
    }

    #[test]
    fn avoid_wrapping_filter_non_string_passes_through() {
        let out = avoid_wrapping_filter(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    #[test]
    fn register_filters_makes_avoid_wrapping_callable_via_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s | avoid_wrapping | safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "June 5");
        assert_eq!(tera.render("t", &ctx).unwrap(), "June\u{00A0}5");
    }

    // -------- add --------

    #[test]
    fn add_sums_two_integers() {
        let out = add(&json!(4), &args_pos(json!(5))).unwrap();
        assert_eq!(out, json!(9));
    }

    #[test]
    fn add_sums_two_floats() {
        let out = add(&json!(1.5), &args_pos(json!(2.25))).unwrap();
        assert_eq!(out, json!(3.75));
    }

    #[test]
    fn add_concatenates_strings() {
        let out = add(&json!("abc"), &args_pos(json!("def"))).unwrap();
        assert_eq!(out, json!("abcdef"));
    }

    #[test]
    fn add_concatenates_arrays() {
        let out = add(&json!([1, 2]), &args_pos(json!([3, 4]))).unwrap();
        assert_eq!(out, json!([1, 2, 3, 4]));
    }

    #[test]
    fn add_mixed_types_stringifies_concat() {
        // "5" + 3 → "53". Both sides stringify, then
        // concatenate.
        let out = add(&json!("5"), &args_pos(json!(3))).unwrap();
        assert_eq!(out, json!("53"));
    }

    // -------- cut --------

    #[test]
    fn cut_removes_every_occurrence_of_needle() {
        let out = cut(&json!("Hello, world"), &args_pos(json!("l"))).unwrap();
        assert_eq!(out, json!("Heo, word"));
    }

    #[test]
    fn cut_handles_multichar_needle() {
        let out = cut(&json!("abc abc"), &args_pos(json!("abc"))).unwrap();
        assert_eq!(out, json!(" "));
    }

    #[test]
    fn cut_empty_needle_returns_input_unchanged() {
        // Guard against infinite-replace loops and against silently
        // gluing every empty position; an empty needle is a no-op.
        let out = cut(&json!("hello"), &args_pos(json!(""))).unwrap();
        assert_eq!(out, json!("hello"));
    }

    #[test]
    fn cut_non_string_value_passes_through() {
        let out = cut(&json!(42), &args_pos(json!("x"))).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- divisibleby --------

    #[test]
    fn divisibleby_true_when_evenly_divisible() {
        let out = divisibleby(&json!(6), &args_pos(json!(3))).unwrap();
        assert_eq!(out, json!(true));
    }

    #[test]
    fn divisibleby_false_with_remainder() {
        let out = divisibleby(&json!(7), &args_pos(json!(3))).unwrap();
        assert_eq!(out, json!(false));
    }

    #[test]
    fn divisibleby_false_on_zero_divisor() {
        // Don't blow up the template on a typoed `divisibleby:0` —
        // false is the safer fall-through.
        let out = divisibleby(&json!(5), &args_pos(json!(0))).unwrap();
        assert_eq!(out, json!(false));
    }

    #[test]
    fn divisibleby_handles_zero_value() {
        // 0 is divisible by every non-zero integer.
        let out = divisibleby(&json!(0), &args_pos(json!(5))).unwrap();
        assert_eq!(out, json!(true));
    }

    // -------- floatformat --------

    #[test]
    fn floatformat_default_is_one_decimal_with_trailing_drop() {
        // No arg: one decimal, drop if zero.
        let nonzero = floatformat(&json!(34.23234), &HashMap::new()).unwrap();
        assert_eq!(nonzero, json!("34.2"));
        let round = floatformat(&json!(34.0), &HashMap::new()).unwrap();
        assert_eq!(round, json!("34"));
    }

    #[test]
    fn floatformat_positive_arg_keeps_trailing_zeros() {
        let out = floatformat(&json!(34.0), &args_pos(json!(3))).unwrap();
        assert_eq!(out, json!("34.000"));
    }

    #[test]
    fn floatformat_positive_arg_truncates_to_n_decimals() {
        let out = floatformat(&json!(34.23234), &args_pos(json!(3))).unwrap();
        assert_eq!(out, json!("34.232"));
    }

    #[test]
    fn floatformat_negative_arg_drops_trailing_zeros() {
        // -3 means "up to 3 decimals, drop them if they're all zero."
        let zero = floatformat(&json!(34.0), &args_pos(json!(-3))).unwrap();
        assert_eq!(zero, json!("34"));
        let nonzero = floatformat(&json!(34.23234), &args_pos(json!(-3))).unwrap();
        assert_eq!(nonzero, json!("34.232"));
    }

    #[test]
    fn floatformat_passes_non_numeric_through() {
        let out = floatformat(&json!("hi"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("hi"));
    }

    // -------- register_filters: end-to-end --------

    #[test]
    fn register_filters_wires_add_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ n|add(value=5) }}").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &4);
        assert_eq!(tera.render("t", &ctx).unwrap(), "9");
    }

    #[test]
    fn register_filters_wires_cut_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s|cut(needle=\"l\") }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "Hello");
        assert_eq!(tera.render("t", &ctx).unwrap(), "Heo");
    }

    #[test]
    fn register_filters_wires_floatformat_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ n|floatformat(precision=2) }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &3.14159);
        assert_eq!(tera.render("t", &ctx).unwrap(), "3.14");
    }

    // -------- escapejs --------

    #[test]
    fn escapejs_escapes_quotes_and_brackets() {
        let out = escapejs(&json!("<script>alert('xss')</script>"), &HashMap::new()).unwrap();
        // < > ' all become \uXXXX; everything else passes through.
        let s = out.as_str().unwrap().to_owned();
        assert!(!s.contains('<'), "got: {s}");
        assert!(!s.contains('>'), "got: {s}");
        assert!(!s.contains('\''), "got: {s}");
        // Letters / safe punctuation pass through.
        assert!(s.contains("script"));
        assert!(s.contains("alert"));
    }

    #[test]
    fn escapejs_escapes_line_separators() {
        // U+2028 and U+2029 must escape — older JS engines treated
        // them as string-terminators inside string literals.
        let ls = "a\u{2028}b\u{2029}c";
        let out = escapejs(&json!(ls), &HashMap::new()).unwrap();
        let s = out.as_str().unwrap().to_owned();
        assert!(s.contains("\\u2028"));
        assert!(s.contains("\\u2029"));
        assert!(!s.contains('\u{2028}'));
    }

    #[test]
    fn escapejs_escapes_control_chars() {
        let out = escapejs(&json!("a\nb"), &HashMap::new()).unwrap();
        // 0x0A (LF) escapes to the literal sequence backslash-u-0-0-0-A
        // so a newline can't break out of the JS string context.
        let s = out.as_str().unwrap().to_owned();
        assert!(s.contains("\\u000A"), "got: {s}");
        assert!(!s.contains('\n'));
    }

    #[test]
    fn escapejs_passes_non_string_through() {
        let out = escapejs(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- yesno --------

    #[test]
    fn yesno_true_maps_to_first_token() {
        let out = yesno(&json!(true), &args_pos(json!("yes,no"))).unwrap();
        assert_eq!(out, json!("yes"));
    }

    #[test]
    fn yesno_false_maps_to_second_token() {
        let out = yesno(&json!(false), &args_pos(json!("yes,no"))).unwrap();
        assert_eq!(out, json!("no"));
    }

    #[test]
    fn yesno_null_uses_third_token_when_provided() {
        let out = yesno(&Value::Null, &args_pos(json!("yes,no,maybe"))).unwrap();
        assert_eq!(out, json!("maybe"));
    }

    #[test]
    fn yesno_null_falls_back_to_no_when_third_token_omitted() {
        let out = yesno(&Value::Null, &args_pos(json!("yes,no"))).unwrap();
        assert_eq!(out, json!("no"));
    }

    #[test]
    fn yesno_no_arg_defaults_to_yes_no_maybe() {
        let out = yesno(&Value::Null, &HashMap::new()).unwrap();
        assert_eq!(out, json!("maybe"));
    }

    // -------- get_digit --------

    #[test]
    fn get_digit_extracts_rightmost_digit() {
        let out = get_digit(&json!(1234), &args_pos(json!(1))).unwrap();
        assert_eq!(out, json!("4"));
    }

    #[test]
    fn get_digit_extracts_leftmost_digit() {
        let out = get_digit(&json!(1234), &args_pos(json!(4))).unwrap();
        assert_eq!(out, json!("1"));
    }

    #[test]
    fn get_digit_past_leftmost_returns_zero() {
        let out = get_digit(&json!(12), &args_pos(json!(5))).unwrap();
        assert_eq!(out, json!("0"));
    }

    #[test]
    fn get_digit_invalid_index_returns_value_unchanged() {
        let out = get_digit(&json!(12), &args_pos(json!(0))).unwrap();
        assert_eq!(out, json!(12));
        let neg = get_digit(&json!(12), &args_pos(json!(-1))).unwrap();
        assert_eq!(neg, json!(12));
    }

    #[test]
    fn get_digit_non_integer_passes_through() {
        let out = get_digit(&json!("hi"), &args_pos(json!(1))).unwrap();
        assert_eq!(out, json!("hi"));
    }

    // -------- dictsort --------

    #[test]
    fn dictsort_sorts_by_string_key() {
        let input = json!([
            {"name": "Charlie"},
            {"name": "Alice"},
            {"name": "Bob"},
        ]);
        let out = dictsort(&input, &args_pos(json!("name"))).unwrap();
        let arr = out.as_array().unwrap();
        assert_eq!(arr[0]["name"], "Alice");
        assert_eq!(arr[1]["name"], "Bob");
        assert_eq!(arr[2]["name"], "Charlie");
    }

    #[test]
    fn dictsort_sorts_by_numeric_key() {
        let input = json!([
            {"age": 30},
            {"age": 5},
            {"age": 20},
        ]);
        let out = dictsort(&input, &args_pos(json!("age"))).unwrap();
        let arr = out.as_array().unwrap();
        assert_eq!(arr[0]["age"], 5);
        assert_eq!(arr[1]["age"], 20);
        assert_eq!(arr[2]["age"], 30);
    }

    #[test]
    fn dictsort_entries_missing_key_sort_first() {
        // Missing key → treated as null → ranks lowest.
        let input = json!([
            {"name": "C"},
            {"other": "x"},
            {"name": "A"},
        ]);
        let out = dictsort(&input, &args_pos(json!("name"))).unwrap();
        let arr = out.as_array().unwrap();
        assert!(
            arr[0].get("name").is_none(),
            "missing-key entry should be first"
        );
        assert_eq!(arr[1]["name"], "A");
        assert_eq!(arr[2]["name"], "C");
    }

    #[test]
    fn dictsort_non_list_passes_through() {
        let out = dictsort(&json!({"k": 1}), &args_pos(json!("k"))).unwrap();
        assert_eq!(out, json!({"k": 1}));
    }

    #[test]
    fn dictsort_empty_key_passes_through() {
        let input = json!([{"a": 2}, {"a": 1}]);
        let out = dictsort(&input, &args_pos(json!(""))).unwrap();
        // Unchanged — no sort happened.
        assert_eq!(out, input);
    }

    // -------- register_filters: end-to-end --------

    #[test]
    fn register_filters_wires_yesno_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ b|yesno(choices=\"on,off\") }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("b", &true);
        assert_eq!(tera.render("t", &ctx).unwrap(), "on");
    }

    #[test]
    fn register_filters_wires_get_digit_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ n|get_digit(index=2) }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &567);
        assert_eq!(tera.render("t", &ctx).unwrap(), "6");
    }

    // -------- slugify_unicode --------

    #[test]
    fn slugify_unicode_handles_basic_ascii() {
        let out = slugify_unicode(&json!("Hello World!"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("hello-world"));
    }

    #[test]
    fn slugify_unicode_preserves_non_ascii_letters() {
        let out = slugify_unicode(&json!("Привет мир"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("привет-мир"));
    }

    #[test]
    fn slugify_unicode_lowercases_uppercase_diacritics() {
        let out = slugify_unicode(&json!("CAFÉ AU LAIT"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("café-au-lait"));
    }

    #[test]
    fn slugify_unicode_collapses_punctuation_runs_to_single_dash() {
        let out = slugify_unicode(&json!("a---b___c   d!!!e"), &HashMap::new()).unwrap();
        // `_` is treated as alnum-equivalent; punctuation + space
        // collapses to dashes.
        assert_eq!(out, json!("a-b___c-d-e"));
    }

    #[test]
    fn slugify_unicode_strips_leading_and_trailing_dashes() {
        let out = slugify_unicode(&json!("   hello   "), &HashMap::new()).unwrap();
        assert_eq!(out, json!("hello"));
        let out2 = slugify_unicode(&json!("!!!hi!!!"), &HashMap::new()).unwrap();
        assert_eq!(out2, json!("hi"));
    }

    #[test]
    fn slugify_unicode_keeps_digits_and_underscores() {
        let out = slugify_unicode(&json!("year_2026!post_42"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("year_2026-post_42"));
    }

    #[test]
    fn slugify_unicode_passes_non_string_through() {
        let out = slugify_unicode(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    #[test]
    fn register_filters_wires_slugify_unicode_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s|slugify_unicode }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "Hello World 日本");
        assert_eq!(tera.render("t", &ctx).unwrap(), "hello-world-日本");
    }

    // -------- iriencode --------

    #[test]
    fn iriencode_preserves_ascii_uri_structural_chars() {
        // The URL itself (path / query / fragment delimiters) must
        // survive unchanged — this is the whole point of iriencode.
        let out = iriencode(
            &json!("https://example.com/path/with?q=1&z=2#frag"),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(out, json!("https://example.com/path/with?q=1&z=2#frag"));
    }

    #[test]
    fn iriencode_percent_encodes_non_ascii_bytes() {
        // "café" — the é is 2 bytes in UTF-8 (0xC3, 0xA9), each
        // becomes %C3 %A9.
        let out = iriencode(&json!("/blog/café"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("/blog/caf%C3%A9"));
    }

    #[test]
    fn iriencode_percent_encodes_spaces() {
        // Space is not in the safe set even though it appears in
        // browser URL bars — encode it.
        let out = iriencode(&json!("/path with spaces"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("/path%20with%20spaces"));
    }

    #[test]
    fn iriencode_preserves_already_percent_encoded_input() {
        // Percent sign itself is in the safe set — already-encoded
        // input round-trips unchanged. (Compare to `urlencode`,
        // which would double-encode by escaping the %.)
        let out = iriencode(&json!("/path/caf%C3%A9"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("/path/caf%C3%A9"));
    }

    #[test]
    fn iriencode_passes_non_string_through() {
        let out = iriencode(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    #[test]
    fn register_filters_wires_iriencode_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ url|iriencode|safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("url", "/blog/café?lang=fr");
        assert_eq!(tera.render("t", &ctx).unwrap(), "/blog/caf%C3%A9?lang=fr");
    }

    // -------- wordwrap --------

    #[test]
    fn wordwrap_wraps_at_word_boundaries() {
        let out = wordwrap(&json!("Joel is a slug"), &args_pos(json!(5))).unwrap();
        assert_eq!(out, json!("Joel\nis a\nslug"));
    }

    #[test]
    fn wordwrap_keeps_line_under_width() {
        let out = wordwrap(&json!("one two three"), &args_pos(json!(7))).unwrap();
        assert_eq!(out, json!("one two\nthree"));
    }

    #[test]
    fn wordwrap_passes_short_input_unchanged() {
        let out = wordwrap(&json!("hi"), &args_pos(json!(80))).unwrap();
        assert_eq!(out, json!("hi"));
    }

    #[test]
    fn wordwrap_honors_existing_newlines() {
        // \n in input separates pre-wrapped paragraphs — don't
        // re-flow across them. Width=15: each input line wraps
        // independently. "first paragraph" fits exactly (15 chars);
        // "second paragraph" doesn't (16 chars including the space)
        // so it breaks after "second".
        let out = wordwrap(
            &json!("first paragraph here\nsecond paragraph here"),
            &args_pos(json!(15)),
        )
        .unwrap();
        assert_eq!(out, json!("first paragraph\nhere\nsecond\nparagraph here"));
    }

    #[test]
    fn wordwrap_zero_or_negative_width_passes_through() {
        let zero = wordwrap(&json!("anything goes"), &args_pos(json!(0))).unwrap();
        assert_eq!(zero, json!("anything goes"));
        let neg = wordwrap(&json!("anything goes"), &args_pos(json!(-1))).unwrap();
        assert_eq!(neg, json!("anything goes"));
    }

    #[test]
    fn wordwrap_long_word_stands_alone_no_hyphenation() {
        // A word longer than width is not split; it ends up on a
        // line of its own.
        let out = wordwrap(&json!("a verylongword b"), &args_pos(json!(5))).unwrap();
        assert_eq!(out, json!("a\nverylongword\nb"));
    }

    #[test]
    fn wordwrap_passes_non_string_through() {
        let out = wordwrap(&json!(42), &args_pos(json!(5))).unwrap();
        assert_eq!(out, json!(42));
    }

    #[test]
    fn register_filters_wires_wordwrap_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s|wordwrap(width=5) }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "Joel is a slug");
        assert_eq!(tera.render("t", &ctx).unwrap(), "Joel\nis a\nslug");
    }

    // -------- mask_email --------

    #[test]
    fn mask_email_masks_middle_of_local_part() {
        let out = mask_email(&json!("alice@example.com"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("a***e@example.com"));
    }

    #[test]
    fn mask_email_handles_short_local_parts_gracefully() {
        // Single-char local: just `*`.
        let one = mask_email(&json!("a@example.com"), &HashMap::new()).unwrap();
        assert_eq!(one, json!("*@example.com"));
        // Two-char local: first char + `*`.
        let two = mask_email(&json!("ab@example.com"), &HashMap::new()).unwrap();
        assert_eq!(two, json!("a*@example.com"));
        // Three-char local: first + *** + last.
        let three = mask_email(&json!("abc@example.com"), &HashMap::new()).unwrap();
        assert_eq!(three, json!("a***c@example.com"));
    }

    #[test]
    fn mask_email_handles_empty_local_part() {
        let out = mask_email(&json!("@example.com"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("@example.com"));
    }

    #[test]
    fn mask_email_passes_through_non_email() {
        let out = mask_email(&json!("not-an-email"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("not-an-email"));
    }

    #[test]
    fn mask_email_passes_through_non_string() {
        let out = mask_email(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    #[test]
    fn register_filters_wires_mask_email_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ email|mask_email }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("email", "operator@example.com");
        assert_eq!(tera.render("t", &ctx).unwrap(), "o***r@example.com");
    }

    // -------- mask_card --------

    #[test]
    fn mask_card_keeps_last_four_digits() {
        let out = mask_card(&json!("4111111111111111"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("************1111"));
    }

    #[test]
    fn mask_card_strips_separators_then_masks() {
        let out = mask_card(&json!("4111 1111 1111 1111"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("************1111"));
        let out2 = mask_card(&json!("4111-1111-1111-1111"), &HashMap::new()).unwrap();
        assert_eq!(out2, json!("************1111"));
    }

    #[test]
    fn mask_card_fully_masks_short_input() {
        let out = mask_card(&json!("1234"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("****"));
        let out2 = mask_card(&json!("12"), &HashMap::new()).unwrap();
        assert_eq!(out2, json!("**"));
    }

    #[test]
    fn mask_card_passes_through_non_digit_input() {
        let out = mask_card(&json!("not a card"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("not a card"));
        let out2 = mask_card(&json!("4111-abcd"), &HashMap::new()).unwrap();
        assert_eq!(out2, json!("4111-abcd"));
    }

    #[test]
    fn mask_card_passes_through_non_string() {
        let out = mask_card(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- mask_phone --------

    #[test]
    fn mask_phone_keeps_separators_and_last_four_digits() {
        let out = mask_phone(&json!("+1 415 555 2671"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("+* *** *** 2671"));
        let out2 = mask_phone(&json!("(415) 555-2671"), &HashMap::new()).unwrap();
        assert_eq!(out2, json!("(***) ***-2671"));
    }

    #[test]
    fn mask_phone_handles_bare_digits() {
        let out = mask_phone(&json!("4155552671"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("******2671"));
    }

    #[test]
    fn mask_phone_fully_masks_short_input() {
        let out = mask_phone(&json!("123"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("***"));
    }

    #[test]
    fn mask_phone_passes_through_no_digits() {
        let out = mask_phone(&json!("no digits"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("no digits"));
    }

    #[test]
    fn mask_phone_passes_through_non_string() {
        let out = mask_phone(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- dictsortreversed --------

    #[test]
    fn dictsortreversed_sorts_descending_by_string_key() {
        let input = json!([
            {"name": "Alice"},
            {"name": "Charlie"},
            {"name": "Bob"},
        ]);
        let out = dictsortreversed(&input, &args_pos(json!("name"))).unwrap();
        let arr = out.as_array().unwrap();
        assert_eq!(arr[0]["name"], "Charlie");
        assert_eq!(arr[1]["name"], "Bob");
        assert_eq!(arr[2]["name"], "Alice");
    }

    #[test]
    fn dictsortreversed_sorts_descending_by_numeric_key() {
        let input = json!([
            {"age": 5},
            {"age": 30},
            {"age": 20},
        ]);
        let out = dictsortreversed(&input, &args_pos(json!("age"))).unwrap();
        let arr = out.as_array().unwrap();
        assert_eq!(arr[0]["age"], 30);
        assert_eq!(arr[1]["age"], 20);
        assert_eq!(arr[2]["age"], 5);
    }

    #[test]
    fn dictsortreversed_missing_key_sorts_last() {
        // Reversed: null (missing) ranks lowest, so it goes LAST.
        let input = json!([
            {"name": "A"},
            {"other": "x"},
            {"name": "C"},
        ]);
        let out = dictsortreversed(&input, &args_pos(json!("name"))).unwrap();
        let arr = out.as_array().unwrap();
        assert_eq!(arr[0]["name"], "C");
        assert_eq!(arr[1]["name"], "A");
        assert!(arr[2].get("name").is_none());
    }

    #[test]
    fn dictsortreversed_passes_through_non_list_and_empty_key() {
        let out = dictsortreversed(&json!({"k": 1}), &args_pos(json!("k"))).unwrap();
        assert_eq!(out, json!({"k": 1}));
        let input = json!([{"a": 2}, {"a": 1}]);
        let out2 = dictsortreversed(&input, &args_pos(json!(""))).unwrap();
        assert_eq!(out2, input);
    }

    // -------- oxford_join --------

    #[test]
    fn oxford_join_empty_list_yields_empty_string() {
        let out = oxford_join(&json!([]), &HashMap::new()).unwrap();
        assert_eq!(out, json!(""));
    }

    #[test]
    fn oxford_join_single_item_is_returned_as_is() {
        let out = oxford_join(&json!(["Alice"]), &HashMap::new()).unwrap();
        assert_eq!(out, json!("Alice"));
    }

    #[test]
    fn oxford_join_two_items_uses_and_without_comma() {
        let out = oxford_join(&json!(["Alice", "Bob"]), &HashMap::new()).unwrap();
        assert_eq!(out, json!("Alice and Bob"));
    }

    #[test]
    fn oxford_join_three_items_uses_serial_comma() {
        let out = oxford_join(&json!(["Alice", "Bob", "Carol"]), &HashMap::new()).unwrap();
        assert_eq!(out, json!("Alice, Bob, and Carol"));
    }

    #[test]
    fn oxford_join_many_items_uses_serial_comma() {
        let out = oxford_join(&json!(["one", "two", "three", "four"]), &HashMap::new()).unwrap();
        assert_eq!(out, json!("one, two, three, and four"));
    }

    #[test]
    fn oxford_join_with_custom_conjunction() {
        let out = oxford_join(&json!(["red", "green", "blue"]), &args_pos(json!("or"))).unwrap();
        assert_eq!(out, json!("red, green, or blue"));
    }

    #[test]
    fn oxford_join_passes_through_non_array() {
        let out = oxford_join(&json!("not a list"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("not a list"));
    }

    // -------- initials --------

    #[test]
    fn initials_extracts_first_char_of_each_word() {
        let out = initials(&json!("Alice Bob"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("AB"));
    }

    #[test]
    fn initials_uppercases_lowercase_input() {
        let out = initials(&json!("alice m. bob"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("AMB"));
    }

    #[test]
    fn initials_single_word_yields_one_char() {
        let out = initials(&json!("Alice"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("A"));
    }

    #[test]
    fn initials_skips_leading_non_alpha_in_word() {
        // "123 Alice" → "A" (digit word produces nothing, second
        // word contributes its first letter).
        let out = initials(&json!("123 Alice"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("A"));
    }

    #[test]
    fn initials_count_limits_result() {
        let out = initials(&json!("alice m. bob"), &args_pos(json!(2))).unwrap();
        assert_eq!(out, json!("AM"));
    }

    #[test]
    fn initials_handles_unicode_letters() {
        let out = initials(&json!("ümlaut éclair"), &HashMap::new()).unwrap();
        // Unicode uppercase.
        assert_eq!(out, json!("ÜÉ"));
    }

    #[test]
    fn initials_empty_input_yields_empty() {
        let out = initials(&json!(""), &HashMap::new()).unwrap();
        assert_eq!(out, json!(""));
    }

    #[test]
    fn initials_passes_through_non_string() {
        let out = initials(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- truncatechars --------

    #[test]
    fn truncatechars_keeps_input_under_or_equal_to_count() {
        let out = truncatechars(&json!("Hi"), &args_pos(json!(10))).unwrap();
        assert_eq!(out, json!("Hi"));
        // Boundary: exactly count chars.
        let boundary = truncatechars(&json!("abc"), &args_pos(json!(3))).unwrap();
        assert_eq!(boundary, json!("abc"));
    }

    #[test]
    fn truncatechars_appends_ellipsis_within_budget() {
        // 14 chars in, count=7 → 6 chars + … = 7 total.
        let out = truncatechars(&json!("Joel is a slug"), &args_pos(json!(7))).unwrap();
        assert_eq!(out, json!("Joel i…"));
        // 4 chars in, count=3 → 2 chars + … = 3 total.
        let three = truncatechars(&json!("abcd"), &args_pos(json!(3))).unwrap();
        assert_eq!(three, json!("ab…"));
    }

    #[test]
    fn truncatechars_zero_count_returns_empty() {
        let out = truncatechars(&json!("anything"), &args_pos(json!(0))).unwrap();
        assert_eq!(out, json!(""));
    }

    #[test]
    fn truncatechars_negative_or_non_int_passes_through() {
        let neg = truncatechars(&json!("anything"), &args_pos(json!(-1))).unwrap();
        assert_eq!(neg, json!("anything"));
        let str_arg = truncatechars(&json!("anything"), &args_pos(json!("five"))).unwrap();
        assert_eq!(str_arg, json!("anything"));
    }

    #[test]
    fn truncatechars_unicode_counts_chars_not_bytes() {
        // "éé" is 2 chars (4 bytes). count=2 means no truncation.
        let out = truncatechars(&json!("éé"), &args_pos(json!(2))).unwrap();
        assert_eq!(out, json!("éé"));
        // "ééé" with count=2 → 1 char + … = 2 chars total.
        let trunc = truncatechars(&json!("ééé"), &args_pos(json!(2))).unwrap();
        assert_eq!(trunc, json!("é…"));
    }

    #[test]
    fn truncatechars_passes_through_non_string() {
        let out = truncatechars(&json!(42), &args_pos(json!(2))).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- normalize_whitespace --------

    #[test]
    fn normalize_whitespace_collapses_runs_of_spaces() {
        let out = normalize_whitespace(&json!("  hello   world  "), &HashMap::new()).unwrap();
        assert_eq!(out, json!("hello world"));
    }

    #[test]
    fn normalize_whitespace_collapses_tabs_and_newlines() {
        let out = normalize_whitespace(&json!("line\n\n\twith tabs"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("line with tabs"));
    }

    #[test]
    fn normalize_whitespace_trims_leading_and_trailing() {
        let out = normalize_whitespace(&json!("\t  hello  \n"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("hello"));
    }

    #[test]
    fn normalize_whitespace_empty_input_yields_empty() {
        let out = normalize_whitespace(&json!(""), &HashMap::new()).unwrap();
        assert_eq!(out, json!(""));
        let blank = normalize_whitespace(&json!("   \t\n"), &HashMap::new()).unwrap();
        assert_eq!(blank, json!(""));
    }

    #[test]
    fn normalize_whitespace_passes_through_non_string() {
        let out = normalize_whitespace(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- wordcount --------

    #[test]
    fn wordcount_counts_whitespace_tokens() {
        let out = wordcount(&json!("Joel is a slug"), &HashMap::new()).unwrap();
        assert_eq!(out, json!(4));
    }

    #[test]
    fn wordcount_empty_is_zero() {
        let out = wordcount(&json!(""), &HashMap::new()).unwrap();
        assert_eq!(out, json!(0));
    }

    #[test]
    fn wordcount_collapses_consecutive_whitespace() {
        let out = wordcount(&json!("a   b\t\tc\nd"), &HashMap::new()).unwrap();
        assert_eq!(out, json!(4));
    }

    // -------- phone2numeric --------

    #[test]
    fn phone2numeric_canonical_example() {
        let out = phone2numeric(&json!("1-800-COLLECT"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("1-800-2655328"));
    }

    #[test]
    fn phone2numeric_is_case_insensitive() {
        let out = phone2numeric(&json!("abcDEF"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("222333"));
    }

    #[test]
    fn phone2numeric_passes_non_letters_through() {
        // Digits, dashes, spaces should be preserved.
        let out = phone2numeric(&json!("(555) 867-5309"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("(555) 867-5309"));
    }

    #[test]
    fn phone2numeric_maps_all_letters_correctly() {
        // ITU E.161 — z → 9 specifically (some old US phones omitted z).
        let out = phone2numeric(&json!("xyz"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("999"));
        let out = phone2numeric(&json!("pqrs"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("7777"));
    }

    // -------- linenumbers --------

    #[test]
    fn linenumbers_simple_three_lines() {
        let out = linenumbers(&json!("one\ntwo\nthree"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("1. one\n2. two\n3. three"));
    }

    #[test]
    fn linenumbers_width_grows_with_count() {
        // 10 lines → width 2.
        let input: String = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = linenumbers(&json!(input), &HashMap::new()).unwrap();
        let text = out.as_str().unwrap();
        assert!(text.starts_with(" 1. line1\n"));
        assert!(text.ends_with("\n10. line10"));
    }

    #[test]
    fn linenumbers_empty_string_is_single_empty_line() {
        let out = linenumbers(&json!(""), &HashMap::new()).unwrap();
        assert_eq!(out, json!("1. "));
    }

    // -------- ljust / rjust / center --------

    #[test]
    fn ljust_pads_right_with_spaces() {
        let out = ljust(&json!("Joel"), &args1("width", 10)).unwrap();
        assert_eq!(out, json!("Joel      "));
    }

    #[test]
    fn ljust_passes_through_already_wide_enough() {
        let out = ljust(&json!("Joel"), &args1("width", 3)).unwrap();
        assert_eq!(out, json!("Joel"));
    }

    #[test]
    fn rjust_pads_left_with_spaces() {
        let out = rjust(&json!("Joel"), &args1("width", 10)).unwrap();
        assert_eq!(out, json!("      Joel"));
    }

    #[test]
    fn center_pads_both_sides_extra_on_right() {
        // 10 - 4 = 6 → 3 left, 3 right.
        let out = center_filter(&json!("Joel"), &args1("width", 10)).unwrap();
        assert_eq!(out, json!("   Joel   "));
    }

    #[test]
    fn center_odd_padding_extra_goes_right() {
        // 9 - 4 = 5 → 2 left, 3 right (matches Python str.center).
        let out = center_filter(&json!("Joel"), &args1("width", 9)).unwrap();
        assert_eq!(out, json!("  Joel   "));
    }

    fn args1(key: &str, value: u64) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(key.to_owned(), json!(value));
        m
    }

    // -------- striptags --------

    #[test]
    fn striptags_removes_tags() {
        let out = striptags(&json!("<p>Hello <b>world</b></p>"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("Hello world"));
    }

    #[test]
    fn register_filters_wires_striptags_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ value|striptags }}").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("value", "<a href='/x'>link</a>");
        assert_eq!(tera.render("t", &ctx).unwrap(), "link");
    }

    // -------- capfirst --------

    #[test]
    fn capfirst_capitalizes_first_char_only() {
        let out = capfirst(&json!("hello WORLD"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("Hello WORLD"));
    }

    #[test]
    fn register_filters_wires_capfirst_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ value|capfirst }}").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("value", "hello");
        assert_eq!(tera.render("t", &ctx).unwrap(), "Hello");
    }

    // -------- addslashes --------

    #[test]
    fn addslashes_escapes_backslash_and_quotes() {
        let out = addslashes(&json!(r#"He said "hi" and 'bye' \ done"#), &HashMap::new()).unwrap();
        assert_eq!(out, json!(r#"He said \"hi\" and \'bye\' \\ done"#));
    }

    #[test]
    fn addslashes_passes_through_clean_strings() {
        let out = addslashes(&json!("plain text"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("plain text"));
    }

    #[test]
    fn register_filters_wires_addslashes_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ value|addslashes|safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("value", r#"O'Brien"#);
        assert_eq!(tera.render("t", &ctx).unwrap(), r#"O\'Brien"#);
    }

    // -------- filesizeformat --------

    #[test]
    fn filesizeformat_renders_kb() {
        let out = filesizeformat(&json!(2048), &HashMap::new()).unwrap();
        // naturalsize uses decimal-binary "1.0 KB" / "2.0 KB" etc.
        let s = out.as_str().unwrap();
        assert!(s.contains("KB"), "got: {}", s);
    }

    #[test]
    fn filesizeformat_passes_through_string_numbers() {
        let out = filesizeformat(&json!("1024"), &HashMap::new()).unwrap();
        let s = out.as_str().unwrap();
        assert!(s.contains("KB"), "got: {}", s);
    }

    #[test]
    fn register_filters_wires_filesizeformat_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ size|filesizeformat }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("size", &1_500_000_i64);
        let out = tera.render("t", &ctx).unwrap();
        assert!(out.contains("MB"), "got: {}", out);
    }

    // -------- length_is --------

    #[test]
    fn length_is_true_when_string_chars_match() {
        let out = length_is(&json!("hello"), &args1("arg", 5)).unwrap();
        assert_eq!(out, json!(true));
    }

    #[test]
    fn length_is_false_when_mismatched() {
        let out = length_is(&json!("hello"), &args1("arg", 4)).unwrap();
        assert_eq!(out, json!(false));
    }

    #[test]
    fn length_is_handles_arrays() {
        let out = length_is(&json!([1, 2, 3]), &args1("arg", 3)).unwrap();
        assert_eq!(out, json!(true));
    }

    // -------- make_list --------

    #[test]
    fn make_list_splits_string_into_chars() {
        let out = make_list(&json!("abc"), &HashMap::new()).unwrap();
        assert_eq!(out, json!(["a", "b", "c"]));
    }

    #[test]
    fn make_list_splits_int_into_digit_chars() {
        let out = make_list(&json!(123), &HashMap::new()).unwrap();
        assert_eq!(out, json!(["1", "2", "3"]));
    }

    // -------- pprint --------

    #[test]
    fn pprint_indents_json_object() {
        let out = pprint(&json!({"a": 1, "b": [2, 3]}), &HashMap::new()).unwrap();
        let s = out.as_str().unwrap();
        assert!(s.contains('\n'), "expected multi-line output: {}", s);
    }

    // -------- urlizetrunc --------

    #[test]
    fn urlizetrunc_truncates_long_visible_text() {
        let input = "Visit https://example.com/some/very/long/path now";
        let out = urlizetrunc(&json!(input), &args1("arg", 15))
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert!(out.contains("..."), "expected ellipsis in: {}", out);
        assert!(
            out.contains("https://example.com/some/very/long/path"),
            "href intact in: {}",
            out
        );
    }

    #[test]
    fn urlizetrunc_leaves_short_text_alone() {
        let input = "Visit https://x.io";
        let out = urlizetrunc(&json!(input), &args1("arg", 100))
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!out.contains("..."), "got: {}", out);
    }

    #[test]
    fn register_filters_wires_urlizetrunc_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ value|urlizetrunc(arg=10)|safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("value", "see https://example.com/about");
        let out = tera.render("t", &ctx).unwrap();
        assert!(out.contains("..."), "got: {}", out);
    }

    // -------- unordered_list --------

    #[test]
    fn unordered_list_renders_flat_list() {
        let out = unordered_list(&json!(["a", "b", "c"]), &HashMap::new())
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(out, "<li>a</li><li>b</li><li>c</li>");
    }

    #[test]
    fn unordered_list_renders_nested_levels() {
        // A label followed by an optional sub-array.
        let input = json!(["States", ["Kansas", ["Lawrence", "Topeka"], "Illinois"]]);
        let out = unordered_list(&input, &HashMap::new())
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert!(out.contains("<li>States"), "got: {}", out);
        assert!(out.contains("<li>Kansas"), "got: {}", out);
        assert!(out.contains("<li>Lawrence</li>"), "got: {}", out);
        assert!(out.contains("<li>Topeka</li>"), "got: {}", out);
        assert!(out.contains("<li>Illinois</li>"), "got: {}", out);
        // Nesting structure preserved.
        assert!(out.contains("<ul>"), "expected nested <ul>: {}", out);
    }

    #[test]
    fn unordered_list_escapes_html_in_labels() {
        let out = unordered_list(&json!(["<script>"]), &HashMap::new())
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert!(out.contains("&lt;script&gt;"), "got: {}", out);
        assert!(!out.contains("<script>"), "raw tag escaped");
    }

    #[test]
    fn register_filters_wires_unordered_list_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ items|unordered_list|safe }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("items", &json!(["a", "b"]));
        assert_eq!(tera.render("t", &ctx).unwrap(), "<li>a</li><li>b</li>");
    }

    // -------- json_script --------

    #[test]
    fn json_script_wraps_value_in_script_tag() {
        let out = json_script(&json!({"x": 1}), &args1_str("arg", "data"))
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert!(out.starts_with(r#"<script id="data" type="application/json">"#));
        assert!(out.ends_with("</script>"));
    }

    #[test]
    fn json_script_defangs_html_breakout() {
        // String value containing `<` `>` `&` must be \u-escaped so a
        // closing </script> inside the value can't break out.
        let out = json_script(&json!("<script>alert(1)</script>"), &args1_str("arg", "x"))
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!out.contains("</script>alert"), "got: {}", out);
        assert!(out.contains("\\u003C"), "got: {}", out);
    }

    #[test]
    fn json_script_defaults_element_id() {
        let out = json_script(&json!(1), &HashMap::new())
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert!(out.contains(r#"id="id""#), "got: {}", out);
    }

    #[test]
    fn register_filters_wires_json_script_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", r#"{{ data|json_script(arg="x")|safe }}"#)
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("data", &json!({"y": 2}));
        let out = tera.render("t", &ctx).unwrap();
        assert!(out.starts_with(r#"<script id="x" type="application/json">"#));
    }

    fn args1_str(key: &str, value: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(key.to_owned(), json!(value));
        m
    }

    // -------- is_blank --------

    #[test]
    fn is_blank_filter_recognizes_blank_strings() {
        assert_eq!(
            is_blank_filter(&json!(""), &HashMap::new()).unwrap(),
            json!(true)
        );
        assert_eq!(
            is_blank_filter(&json!("   "), &HashMap::new()).unwrap(),
            json!(true)
        );
        assert_eq!(
            is_blank_filter(&json!("\t\n"), &HashMap::new()).unwrap(),
            json!(true)
        );
        assert_eq!(
            is_blank_filter(&json!(null), &HashMap::new()).unwrap(),
            json!(true)
        );
    }

    #[test]
    fn is_blank_filter_rejects_content() {
        assert_eq!(
            is_blank_filter(&json!("hello"), &HashMap::new()).unwrap(),
            json!(false)
        );
        assert_eq!(
            is_blank_filter(&json!("  x  "), &HashMap::new()).unwrap(),
            json!(false)
        );
        // Non-string scalars aren't "blank" — only strings are.
        assert_eq!(
            is_blank_filter(&json!(0), &HashMap::new()).unwrap(),
            json!(false)
        );
        assert_eq!(
            is_blank_filter(&json!(false), &HashMap::new()).unwrap(),
            json!(false)
        );
    }

    #[test]
    fn register_filters_wires_is_blank_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{% if v|is_blank %}empty{% else %}filled{% endif %}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("v", "   ");
        assert_eq!(tera.render("t", &ctx).unwrap(), "empty");
        let mut ctx = tera::Context::new();
        ctx.insert("v", "hi");
        assert_eq!(tera.render("t", &ctx).unwrap(), "filled");
    }

    // -------- truncate_middle --------

    #[test]
    fn truncate_middle_filter_uses_default_width_when_missing() {
        let out = truncate_middle_filter(&json!("short"), &HashMap::new()).unwrap();
        assert_eq!(out, json!("short"));
    }

    #[test]
    fn truncate_middle_filter_respects_explicit_width() {
        let mut args = HashMap::new();
        args.insert("width".into(), json!(10));
        let out =
            truncate_middle_filter(&json!("0123456789abcdef0123456789abcdef"), &args).unwrap();
        let s = out.as_str().unwrap();
        assert!(s.contains("…"), "got: {s}");
        assert_eq!(s.chars().count(), 10);
    }

    #[test]
    fn truncate_middle_filter_respects_custom_placeholder() {
        let mut args = HashMap::new();
        args.insert("width".into(), json!(10));
        args.insert("placeholder".into(), json!("..."));
        let out =
            truncate_middle_filter(&json!("0123456789abcdef0123456789abcdef"), &args).unwrap();
        let s = out.as_str().unwrap();
        assert!(s.contains("..."), "got: {s}");
    }

    #[test]
    fn truncate_middle_filter_passes_through_non_string() {
        let out = truncate_middle_filter(&json!(42), &HashMap::new()).unwrap();
        assert_eq!(out, json!(42));
    }

    // -------- widthratio --------

    fn wr_args(value: f64, max: f64, width: f64) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("value".to_owned(), json!(value));
        m.insert("max".to_owned(), json!(max));
        m.insert("width".to_owned(), json!(width));
        m
    }

    #[test]
    fn widthratio_basic_proportional_math() {
        // 50 / 100 * 200 = 100
        let out = widthratio(&wr_args(50.0, 100.0, 200.0)).unwrap();
        assert_eq!(out, json!(100));
    }

    #[test]
    fn widthratio_rounds_to_nearest_int() {
        // 175 / 200 * 100 = 87.5 → 88 (banker's-round)
        let out = widthratio(&wr_args(175.0, 200.0, 100.0)).unwrap();
        assert_eq!(out, json!(88));
    }

    #[test]
    fn widthratio_max_zero_returns_zero() {
        // Defensive: zero-max would divide by zero.
        let out = widthratio(&wr_args(50.0, 0.0, 200.0)).unwrap();
        assert_eq!(out, json!(0));
    }

    #[test]
    fn widthratio_missing_args_return_zero() {
        let out = widthratio(&HashMap::new()).unwrap();
        assert_eq!(out, json!(0));
    }

    #[test]
    fn register_widthratio_through_tera() {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ widthratio(value=count, max=total, width=200) }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("count", &30);
        ctx.insert("total", &120);
        // 30 / 120 * 200 = 50
        assert_eq!(tera.render("t", &ctx).unwrap(), "50");
    }
}
