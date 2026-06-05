//! Django `defaultfilters` template filters as Tera filters. Issue #61.
//!
//! Django built-ins that Tera doesn't ship out of the box and that
//! templates reach for constantly: `pluralize`, `truncatewords`,
//! `linebreaks`, `default_if_none`, `add`, `cut`, `divisibleby`,
//! `floatformat`, `escapejs`, `yesno`, `get_digit`, `dictsort`,
//! `slugify_unicode`, `iriencode`, `wordwrap`, `mask_email`,
//! `mask_card`, `mask_phone`, `dictsortreversed`, `oxford_join`,
//! `initials`, `truncatechars`, `normalize_whitespace`, `wordcount`,
//! `phone2numeric`, `linenumbers`, `ljust`, `rjust`, `center`. Call
//! [`register_filters`] on a Tera instance to make them available:
//!
//! ```ignore
//! let mut tera = tera::Tera::default();
//! rustango::default_filters::register_filters(&mut tera);
//! // now {{ count | pluralize }} renders "" / "s"
//! ```
//!
//! Tera already ships `linebreaksbr`, `striptags`, `truncate`,
//! `wordcount` — those aren't repeated here. This module only adds
//! the *missing* defaultfilters group.
//!
//! Matches [Django defaultfilters](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/)
//! output character-for-character on the English (en-US) locale.

use std::collections::HashMap;

use tera::{to_value, Tera, Value};

/// Register every defaultfilters filter on `tera`. Call from app
/// setup (typically right after `Tera::new(...)` / `Tera::default()`).
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
    tera.register_filter("normalize_whitespace", normalize_whitespace);
    tera.register_filter("wordcount", wordcount);
    tera.register_filter("phone2numeric", phone2numeric);
    tera.register_filter("linenumbers", linenumbers);
    tera.register_filter("ljust", ljust);
    tera.register_filter("rjust", rjust);
    tera.register_filter("center", center_filter);
}

// ------------------------------------------------------------------ pluralize

/// `pluralize` — return the singular/plural suffix that matches an
/// integer-like value. Django:
/// - `{{ 1|pluralize }}` → `""`
/// - `{{ 2|pluralize }}` → `"s"`
/// - `{{ 1|pluralize:"es" }}` → `""`
/// - `{{ 2|pluralize:"es" }}` → `"es"`
/// - `{{ 1|pluralize:"y,ies" }}` → `"y"`
/// - `{{ 2|pluralize:"y,ies" }}` → `"ies"`
///
/// Non-integer / non-collection values panic in Django; we mirror
/// the safer pass-through behaviour and return `""` so a typoed
/// variable doesn't blow up the page.
fn pluralize(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let count = count_for_pluralize(value);
    let suffix_arg = args
        .get("suffix")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("s");
    Ok(to_value(crate::text::pluralize(count, suffix_arg))?)
}

/// Resolve the count that drives pluralize. Django accepts ints,
/// floats, and collections (where len() decides). We mirror that:
/// - integer → use directly
/// - float → truncate to integer
/// - array / map / string → use length
/// - anything else → 0 (so the plural form wins; matches Django's
///   "non-iterable defaults to 0" branch).
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
/// were dropped. Django:
/// - `{{ "Joel is a slug"|truncatewords:2 }}` → `"Joel is …"`
/// - `{{ "two words"|truncatewords:5 }}` → `"two words"`
///
/// Negative / zero / non-integer arguments produce an empty string
/// (matching Django). Whitespace handling: collapse-on-emit so a
/// multi-space input round-trips as single-spaced output.
fn truncatewords(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        // Pass non-string values through unchanged — Django panics
        // here; we prefer not to.
        return Ok(value.clone());
    };
    let n = args
        .get("count")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if n <= 0 {
        return Ok(to_value("")?);
    }
    let n = usize::try_from(n).unwrap_or(0);
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= n {
        // Re-join so multi-space input still normalizes consistently.
        return Ok(to_value(words.join(" "))?);
    }
    let kept = words[..n].join(" ");
    Ok(to_value(format!("{kept} …"))?)
}

// ------------------------------------------------------------------ linebreaks

/// `linebreaks` — turn plain-text line breaks into HTML. Django:
/// blank-line-separated chunks become `<p>` blocks; single newlines
/// inside a chunk become `<br>`. The input is HTML-escaped first so
/// raw `<script>` in user input doesn't leak through.
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
    // Normalize line endings — paragraph splits happen on \n\n
    // regardless of platform.
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let escaped = html_escape(&s);
    let paragraphs: Vec<String> = escaped
        .split("\n\n")
        .filter(|p| !p.is_empty())
        .map(|p| {
            let with_br = p.replace('\n', "<br>");
            format!("<p>{with_br}</p>")
        })
        .collect();
    Ok(to_value(paragraphs.join("\n\n"))?)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

// ------------------------------------------------------------------ default_if_none

/// `default_if_none` — replace `null` with the argument. Distinct
/// from Tera's built-in `default` filter (which replaces *undefined*
/// values). Django:
/// - `{{ user.bio|default_if_none:"(no bio)" }}` → `"(no bio)"` when
///   the bio field is JSON null
/// - `{{ "hello"|default_if_none:"x" }}` → `"hello"`
/// - Empty string is NOT null — it passes through.
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

/// `add` — Django's universal addition filter. Numeric inputs add
/// numerically; string inputs concatenate; mismatched types fall
/// back to a stringified concat. Django:
/// - `{{ 4|add:5 }}` → `"9"`
/// - `{{ "abc"|add:"def" }}` → `"abcdef"`
/// - `{{ [1, 2]|add:[3, 4] }}` → `"[1, 2, 3, 4]"` (list concat)
///
/// We mirror the numeric / string / list-concat paths. Anything
/// that can't be coerced into either side falls back to string
/// concatenation of `to_string()` views — same conservative shape
/// Django takes.
fn add(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let rhs = args.get("value").or_else(|| args.values().next());
    let Some(rhs) = rhs else {
        return Ok(value.clone());
    };
    // Numeric path: both sides have a numeric representation.
    if let (Some(a), Some(b)) = (value.as_i64(), rhs.as_i64()) {
        return Ok(to_value(a + b)?);
    }
    if let (Some(a), Some(b)) = (value.as_f64(), rhs.as_f64()) {
        return Ok(to_value(a + b)?);
    }
    // List-concat path: both sides are arrays.
    if let (Some(a), Some(b)) = (value.as_array(), rhs.as_array()) {
        let mut out = a.clone();
        out.extend(b.iter().cloned());
        return Ok(Value::Array(out));
    }
    // String / mixed path — concatenate the stringified views.
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
/// Django:
/// - `{{ "Hello, world"|cut:"l" }}` → `"Heo, word"`
/// - `{{ "abc abc"|cut:"abc" }}` → `" "` (one space remains)
///
/// Empty argument returns the value unchanged so a typoed
/// `{{ x|cut:"" }}` doesn't infinite-loop or replace every empty
/// position.
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

/// `divisibleby` — `true` when value is evenly divisible by the
/// argument. Django:
/// - `{{ 6|divisibleby:3 }}` → `"True"` (Django renders bool)
/// - `{{ 7|divisibleby:3 }}` → `"False"`
///
/// Non-integer / zero-divisor returns `false`. Most useful in `{% if %}`
/// guards: `{% if forloop.counter|divisibleby:3 %}new row{% endif %}`.
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

/// `floatformat` — Django's locale-aware float formatter. Distinct
/// from generic `round`:
/// - `{{ 34.23234 }}` → `"34.23234"` (no filter, no rounding)
/// - `{{ 34.23234|floatformat }}` → `"34.2"` (default 1 decimal)
/// - `{{ 34.00000|floatformat }}` → `"34"` (trailing zeros dropped
///   when the decimal is exactly zero)
/// - `{{ 34.23234|floatformat:3 }}` → `"34.232"` (N decimals)
/// - `{{ 34.00000|floatformat:3 }}` → `"34.000"` (positive arg keeps
///   trailing zeros)
/// - `{{ 34.23234|floatformat:-3 }}` → `"34.232"` (negative arg
///   drops trailing zeros — `34.0|floatformat:-3` → `"34"`)
///
/// The negative-precision drop is Django's distinguishing trick:
/// `{{ price|floatformat:-2 }}` reads as "two decimals max, hide
/// them when value is a round number." Useful for prices /
/// percentages where `$5.00` should render as `$5`.
fn floatformat(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(f) = value.as_f64() else {
        return Ok(value.clone());
    };
    let precision: i64 = args
        .get("precision")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let abs = precision.unsigned_abs() as usize;
    let drop_trailing = precision <= 0;
    let formatted = format!("{f:.abs$}");
    if drop_trailing {
        // Drop the decimal portion entirely if it's all zeros.
        if let Some((int_part, frac_part)) = formatted.split_once('.') {
            if frac_part.chars().all(|c| c == '0') {
                return Ok(to_value(int_part)?);
            }
        }
    }
    Ok(to_value(formatted)?)
}

// ------------------------------------------------------------------ escapejs

/// `escapejs` — escape a string for safe embedding inside a JS
/// string literal in HTML. Django:
///
/// ```html
/// <script>var s = "{{ value|escapejs }}";</script>
/// ```
///
/// Escapes characters that have either HTML or JS-context meaning,
/// so neither `</script>` injection nor JS-syntax breakage is
/// possible regardless of operator input. Quotes, slashes, brackets,
/// `&`, `=`, `-`, `;`, backticks, line separators (U+2028 / U+2029)
/// and every control character all turn into `\uXXXX` escapes;
/// everything else passes through.
fn escapejs(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::escapejs(s))?)
}

// ------------------------------------------------------------------ yesno

/// `yesno` — three-way string mapper for booleans. Django:
/// - `{{ true|yesno:"yes,no" }}` → `"yes"`
/// - `{{ false|yesno:"yes,no" }}` → `"no"`
/// - `{{ null|yesno:"yes,no,maybe" }}` → `"maybe"`
/// - `{{ null|yesno:"yes,no" }}` → `"no"` (no third token → use "no")
///
/// Argument shape: comma-separated `"yes,no"` or `"yes,no,maybe"`.
/// Missing arg defaults to Django's `"yes,no,maybe"`.
fn yesno(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let raw = args
        .get("choices")
        .or_else(|| args.values().next())
        .and_then(Value::as_str)
        .unwrap_or("yes,no,maybe");
    let mut parts = raw.splitn(3, ',');
    let yes = parts.next().unwrap_or("yes");
    let no = parts.next().unwrap_or("no");
    let maybe = parts.next().unwrap_or(no);
    let pick = if value.is_null() {
        maybe
    } else if value.as_bool().unwrap_or(true) {
        yes
    } else {
        no
    };
    Ok(to_value(pick)?)
}

// ------------------------------------------------------------------ get_digit

/// `get_digit` — extract the Nth digit (1-indexed, from the RIGHT)
/// of an integer. Django:
/// - `{{ 1234|get_digit:1 }}` → `"4"` (rightmost)
/// - `{{ 1234|get_digit:2 }}` → `"3"`
/// - `{{ 1234|get_digit:4 }}` → `"1"`
/// - `{{ 1234|get_digit:5 }}` → `"0"` (past the leftmost digit)
///
/// Non-integer values pass through unchanged. Argument `< 1`
/// returns the value as-is (Django's documented passthrough on
/// invalid index).
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

/// `dictsort` — sort a list of objects by a named key. Django:
/// - `{{ users|dictsort:"name" }}` → list reordered alphabetically
///   by each entry's `name` field
/// - `{{ users|dictsort:"age" }}` → reordered numerically by `age`
///
/// Sort is stable and uses the JSON `Value` Ord we implement here:
/// numbers and booleans first (numerically/by bool order), then
/// strings (lexicographic), then everything else (compared via the
/// JSON string form). Non-list input passes through unchanged.
/// Entries missing the key sort first (treated as `null`).
///
/// Nested key paths (`"address.city"`) are NOT supported in this
/// slice — Django allows dotted paths; we add them when the first
/// caller needs them.
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

/// `dictsortreversed` — Django's `dictsortreversed`. Descending
/// counterpart of [`dictsort`]: stable sort by named key, largest
/// first. Same semantics: missing-key entries treated as null and
/// sort to the END (lowest after reversal). Non-list input passes
/// through. Empty key is a no-op (returns unchanged).
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

/// `oxford_join` — join a list of strings as a natural-language
/// list with the Oxford (serial) comma. Single-arg variant uses
/// the default conjunction `"and"`:
///
/// - `[]` → `""`
/// - `["a"]` → `"a"`
/// - `["a", "b"]` → `"a and b"` (no comma)
/// - `["a", "b", "c"]` → `"a, b, and c"` (Oxford comma)
/// - `["a", "b", "c", "d"]` → `"a, b, c, and d"`
///
/// Two-arg variant lets you switch the conjunction:
///
/// ```jinja
/// {{ items | oxford_join(conj="or") }}  {# "a, b, or c" #}
/// ```
///
/// Non-string list elements get stringified via `to_string()`.
/// Non-array input passes through unchanged.
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
    let out = match items.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} {conj} {b}"),
        rest => {
            let (last, init) = rest.split_last().unwrap();
            let head = init.join(", ");
            format!("{head}, {conj} {last}")
        }
    };
    Ok(to_value(out)?)
}

// ------------------------------------------------------------------ initials

/// `initials` — return the uppercase first character of each
/// whitespace-separated word in the input. Used for the
/// avatar-fallback shape every web app builds (one or two letters
/// inside a colored circle when no profile picture is uploaded).
///
/// Behaviour:
/// - Default: first character of every word, uppercased.
/// - `count` argument: limit to the first N initials.
/// - Non-alphabetic leading chars are skipped so `"123 Alice"`
///   yields `"A"` not `"1"`.
/// - Single string of one char yields that uppercased char.
///
/// Examples:
/// - `"Alice"` → `"A"`
/// - `"Alice Bob"` → `"AB"`
/// - `"alice m. bob"` → `"AMB"`
/// - `"alice m. bob" | initials(count=2)` → `"AM"`
/// - `""` → `""`
fn initials(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let limit = args
        .get("count")
        .or_else(|| args.values().next())
        .and_then(Value::as_i64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let mut out = String::new();
    for word in s.split_whitespace() {
        // Find the first alphabetic char in the word.
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
    Ok(to_value(out)?)
}

// ------------------------------------------------------------------ truncatechars

/// `truncatechars` — Django's `truncatechars`. Truncate the input
/// to at most `count` characters; if any chars were dropped,
/// append `…` (single ellipsis char). The ellipsis counts toward
/// the `count` budget — `truncatechars(5)` produces at most 5
/// chars of output total.
///
/// Distinct from Tera's built-in `truncate` (which adds the
/// ellipsis BEYOND the count, and uses the literal `...`).
///
/// Examples:
/// - `"Joel is a slug" | truncatechars(count=7)` → `"Joel i…"` (7 chars)
/// - `"Hi" | truncatechars(count=10)` → `"Hi"` (no truncation needed)
/// - `"abc" | truncatechars(count=3)` → `"abc"` (boundary)
/// - `"abcd" | truncatechars(count=3)` → `"ab…"` (3 chars total)
/// - `"any" | truncatechars(count=0)` → `""` (zero budget)
///
/// Negative / non-integer `count` returns the input unchanged.
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
    let total = s.chars().count();
    if total <= n {
        return Ok(value.clone());
    }
    if n == 0 {
        return Ok(to_value("")?);
    }
    // Reserve 1 char for the ellipsis.
    let keep = n.saturating_sub(1);
    let truncated: String = s.chars().take(keep).collect();
    Ok(to_value(format!("{truncated}…"))?)
}

// ------------------------------------------------------------------ normalize_whitespace

/// `normalize_whitespace` — collapse any run of whitespace
/// (spaces, tabs, newlines, NBSP, etc.) into a single space, and
/// trim leading + trailing whitespace.
///
/// Useful for sanitizing free-text fields before display in a
/// single-line context (admin list columns, email subjects, page
/// titles) where the original line breaks shouldn't show.
///
/// - `"  hello   world  "` → `"hello world"`
/// - `"line\n\n\twith tabs"` → `"line with tabs"`
/// - `""` → `""`
///
/// Non-string input passes through unchanged.
fn normalize_whitespace(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::normalize_whitespace(s))?)
}

/// Total ordering across heterogeneous JSON `Value`s. Null < bool <
/// number < string < array < object. Within a type, use the type's
/// natural ordering (numeric for numbers, lexicographic for strings).
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
        // Arrays + Objects: fall back to JSON-stringified compare so
        // the sort stays deterministic. Rarely needed in practice.
        _ => a.to_string().cmp(&b.to_string()),
    }
}

// ------------------------------------------------------------------ slugify_unicode

/// `slugify_unicode` — Django's `slugify(allow_unicode=True)` variant.
/// Convert a value to a URL-safe slug while preserving non-ASCII
/// letters. Useful for blog-post slugs / handles in apps that serve
/// users typing in scripts other than Latin.
///
/// Behaviour:
/// - lowercase everything,
/// - keep Unicode letters / digits and `_`,
/// - collapse runs of whitespace / hyphens / other punctuation into
///   a single `-`,
/// - strip leading + trailing `-`.
///
/// ```jinja
/// {{ "Hello World!" | slugify_unicode }}   {# → "hello-world" #}
/// {{ "Привет мир"   | slugify_unicode }}   {# → "привет-мир" #}
/// {{ "café-au-lait" | slugify_unicode }}   {# → "café-au-lait" #}
/// ```
///
/// Tera ships an ASCII-only `slugify` already — that filter
/// transliterates non-ASCII to ASCII or drops it. Use this one
/// when the project actively wants Unicode in URLs.
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
        // Anything else when last_was_dash is true: skip (collapse run).
        // Same when the output is empty (no leading dash).
    }
    while out.ends_with('-') {
        out.pop();
    }
    Ok(to_value(out)?)
}

// ------------------------------------------------------------------ iriencode

/// `iriencode` — Django's encoder for [IRIs](https://tools.ietf.org/html/rfc3987).
/// Percent-encodes only the bytes that aren't valid in a URI:
/// non-ASCII characters and a handful of reserved-but-unsafe ones.
/// Everything else (`/`, `:`, `?`, `#`, `=`, `&`, `-`, `_`, `.`,
/// `~`, etc.) passes through unchanged.
///
/// Useful for href attributes when you already have a URL with
/// non-ASCII content (a hash, a translated path) and want it
/// browser-safe without mangling the URL structure:
///
/// ```jinja
/// <a href="{{ url | iriencode }}">link</a>
/// ```
///
/// Distinct from Tera's `urlencode` which is for query-string
/// VALUES — it percent-encodes everything except `[a-zA-Z0-9_-]`
/// (no `/`, no `:`, etc.), so passing a URL through `urlencode`
/// breaks the URL structure. Use `iriencode` for href / src
/// attributes; use `urlencode` for individual query-string values.
fn iriencode(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        // RFC 3987 + Django's safe set: keep unreserved + most
        // reserved chars (those that are syntactically meaningful
        // in URIs). Encode everything else.
        let safe = matches!(
            byte,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~'
                | b'/' | b':' | b'?' | b'#' | b'[' | b']' | b'@'
                | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
                | b'*' | b'+' | b',' | b';' | b'=' | b'%'
        );
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(to_value(out)?)
}

// ------------------------------------------------------------------ wordwrap

/// `wordwrap` — wrap text at word boundaries so no rendered line
/// exceeds `width` columns. Django:
///
/// - `{{ "Joel is a slug"|wordwrap:5 }}` → `"Joel\nis a\nslug"`
/// - `{{ "one two three"|wordwrap:7 }}` → `"one two\nthree"`
///
/// Useful for plain-text emails / SMS / fixed-width displays.
/// Existing `\n` newlines in the input are honored — content
/// already wrapped never gets re-flowed across an explicit line
/// break.
///
/// Words longer than `width` are *not* hyphenated — they end up
/// on a line of their own (same as Django's textwrap-backed
/// behaviour). `width <= 0` returns the input unchanged.
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
    // Honor explicit \n in the input by wrapping each line independently
    // and re-joining. Empty lines stay empty (preserves paragraph breaks).
    let wrapped = s
        .split('\n')
        .map(|line| wrap_one_line(line, width))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(to_value(wrapped)?)
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
        // Would this word push the line past width?
        let proposed = current_len + 1 + word_chars; // " " + word
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

// ------------------------------------------------------------------ mask_email

/// `mask_email` — render a partly-obscured email address for
/// display in admin lists / audit logs where leaking the full
/// address would be a privacy / PII concern.
///
/// Format: first + last char of local part stay; middle is
/// replaced with three `*`. Domain is unchanged. Local parts of
/// 0–2 chars degrade gracefully (no double-show).
///
/// - `alice@example.com` → `a***e@example.com`
/// - `bob@example.com` → `b***b@example.com`
/// - `a@example.com` → `*@example.com`
/// - `@example.com` → `@example.com` (empty local, unchanged shape)
/// - `not-an-email` → `not-an-email` (no `@`, passes through)
///
/// Pure transform — emits the masked string. Doesn't validate the
/// input is actually a valid email (use [`crate::validators::validate_email`]
/// before storage if that matters).
fn mask_email(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    // Preserve pass-through behavior for non-email shapes (no `@`)
    // so a non-email passing through the filter doesn't transform.
    if !s.contains('@') {
        return Ok(value.clone());
    }
    Ok(to_value(crate::text::mask_email(s))?)
}

// ------------------------------------------------------------------ mask_card

/// `mask_card` — render the canonical "************1234"-style
/// masked credit card number from a digit string. Strips spaces
/// and hyphens (typical human-typed shape) before masking.
///
/// Format: every digit except the LAST 4 is replaced with `*`.
/// If the input has fewer than 5 digits, the whole thing is
/// masked. Non-digit input passes through unchanged.
///
/// - `"4111 1111 1111 1111"` → `"************1111"`
/// - `"4111111111111111"` → `"************1111"`
/// - `"4111"` → `"****"` (≤ 4 digits, fully masked)
/// - `"not a card"` → `"not a card"` (no digits, passes through)
///
/// Pair with [`crate::validators::validate_creditcard_luhn`] at
/// intake; use this filter at render time when displaying a
/// stored / processed card for confirmation. Helpful in admin
/// UIs that show order details with a "card on file" line.
fn mask_card(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::mask_card(s))?)
}

// ------------------------------------------------------------------ mask_phone

/// `mask_phone` — render a partly-obscured phone number. Keeps
/// the original separator characters in place; masks every digit
/// except the last 4. Useful for admin lists / order summaries
/// where a full phone number would be PII over-share.
///
/// - `"+1 415 555 2671"` → `"+* *** *** 2671"`
/// - `"(415) 555-2671"` → `"(***) ***-2671"`
/// - `"4155552671"` → `"******2671"`
/// - `"123"` → `"***"` (≤ 4 digits → all masked)
/// - `"no digits"` → `"no digits"` (passes through)
///
/// Non-string passes through unchanged.
fn mask_phone(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    Ok(to_value(crate::text::mask_phone(s))?)
}

// ------------------------------------------------------------------ wordcount

/// `wordcount` — count whitespace-separated tokens in the input.
/// Django:
/// - `{{ "Joel is a slug"|wordcount }}` → `4`
/// - `{{ ""|wordcount }}` → `0`
fn wordcount(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    Ok(to_value(crate::text::wordcount(s))?)
}

// ------------------------------------------------------------------ phone2numeric

/// `phone2numeric` — convert phone-keypad letters to the matching digit.
/// Django:
/// - `{{ "1-800-COLLECT"|phone2numeric }}` → `"1-800-2655328"`
///
/// Mapping (lower + upper, standard ITU E.161):
/// `abc → 2`, `def → 3`, `ghi → 4`, `jkl → 5`, `mno → 6`,
/// `pqrs → 7`, `tuv → 8`, `wxyz → 9`. Non-letter characters pass
/// through unchanged.
fn phone2numeric(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(s) = value.as_str() else {
        return Ok(value.clone());
    };
    let out: String = s
        .chars()
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
        .collect();
    Ok(to_value(out)?)
}

// ------------------------------------------------------------------ linenumbers

/// `linenumbers` — prepend each line of the input with its 1-based
/// line number, zero-padded to the width of the largest line number.
/// Django:
/// - `{{ "one\ntwo\nthree"|linenumbers }}` →
///   `"1. one\n2. two\n3. three"`
///
/// Width adjusts as the line count grows — 100 lines render with
/// `"  1. …"` through `"100. …"`.
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

/// `ljust:N` — left-justify (pad right with spaces) to width N.
/// Django:
/// - `{{ "Joel"|ljust:10 }}` → `"Joel      "`
///
/// Values already at or beyond N pass through unchanged.
fn ljust(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let width = pad_arg(args);
    Ok(to_value(crate::text::ljust(s, width))?)
}

/// `rjust:N` — right-justify (pad left with spaces) to width N.
/// Django:
/// - `{{ "Joel"|rjust:10 }}` → `"      Joel"`
fn rjust(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let width = pad_arg(args);
    Ok(to_value(crate::text::rjust(s, width))?)
}

/// `center:N` — center value in a field of width N. Django:
/// - `{{ "Joel"|center:10 }}` → `"   Joel   "`
///
/// When the padding doesn't split evenly, the extra space goes on
/// the right (matching Django's `str.center`).
fn center_filter(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let width = pad_arg(args);
    Ok(to_value(crate::text::center(s, width))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------- pluralize --------

    fn args_pos(v: Value) -> HashMap<String, Value> {
        // Tera passes positional filter args via the "0" key (or any
        // key — Django's `:arg` becomes a single named arg in Tera's
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
        // Django: passing a list runs pluralize against len(list).
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
        // Django normalizes whitespace on join. Input has tabs +
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
        // Empty string is a real value — passes through. Distinct
        // from Django's `default` filter which treats falsy as
        // missing.
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
        // "5" + 3 → "53" (Django shape). Both sides stringify, then
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
        // gluing every empty position; Django no-ops on empty needle.
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
}
