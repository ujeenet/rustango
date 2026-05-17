//! Django `defaultfilters` template filters as Tera filters. Issue #61.
//!
//! Django built-ins that Tera doesn't ship out of the box and that
//! templates reach for constantly: `pluralize`, `truncatewords`,
//! `linebreaks`, `default_if_none`, `add`, `cut`, `divisibleby`,
//! `floatformat`. Call [`register_filters`] on a Tera instance to
//! make them available:
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
    let (singular, plural) = parse_pluralize_arg(suffix_arg);
    let out = if count == 1 { singular } else { plural };
    Ok(to_value(out)?)
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

/// Split a pluralize argument into `(singular, plural)`. Empty
/// argument defaults to `"" / "s"` (Django default). One token
/// means `"" / token`. Two comma-separated tokens are
/// `(singular, plural)`. Anything beyond two tokens uses only the
/// first two (Django ignores extras silently).
fn parse_pluralize_arg(arg: &str) -> (String, String) {
    let parts: Vec<&str> = arg.split(',').collect();
    match parts.as_slice() {
        [""] | [] => (String::new(), "s".to_owned()),
        [one] => (String::new(), (*one).to_owned()),
        [singular, plural, ..] => ((*singular).to_owned(), (*plural).to_owned()),
    }
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
    if needle.is_empty() {
        return Ok(value.clone());
    }
    Ok(to_value(s.replace(needle, ""))?)
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
}
