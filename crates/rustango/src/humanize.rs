//! Django `humanize` template filters as Tera filters. Issue #17.
//!
//! Seven filters that show up on every user-facing template:
//! `intcomma`, `intword`, `naturalsize`, `ordinal`, `apnumber`,
//! `naturaltime`, `naturalday`. Call [`register_filters`] on a Tera
//! instance to make them available:
//!
//! ```ignore
//! let mut tera = tera::Tera::default();
//! rustango::humanize::register_filters(&mut tera);
//! // now {{ count | intcomma }} renders "1,234,567"
//! ```
//!
//! Matches [Django humanize](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/)
//! output character-for-character on the English (en-US) locale.
//! Locale-aware formatting (German thousands-separator `.`, French
//! `intword` plural words, etc.) is deferred — gated on the
//! framework-wide timezone / locale issue. Until then, every filter
//! emits English output.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Utc};
use tera::{to_value, Tera, Value};

/// Register every humanize filter on `tera`. Call from app setup
/// (typically right after `Tera::new(...)` / `Tera::default()`).
pub fn register_filters(tera: &mut Tera) {
    tera.register_filter("intcomma", intcomma_filter);
    tera.register_filter("intword", intword_filter);
    tera.register_filter("naturalsize", naturalsize_filter);
    tera.register_filter("ordinal", ordinal_filter);
    tera.register_filter("apnumber", apnumber_filter);
    tera.register_filter("naturaltime", naturaltime_filter);
    tera.register_filter("naturalday", naturalday_filter);
    tera.register_filter("timesince", timesince);
    tera.register_filter("timeuntil", timeuntil);
    tera.register_filter("format_number", format_number_filter);
    tera.register_filter("format_currency", format_currency);
}

// ------------------------------------------------------------------ intcomma

/// [`django.contrib.humanize.intcomma`](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/#intcomma) —
/// insert thousands-separator commas into an integer.
/// `4500 → "4,500"`, `1_234_567 → "1,234,567"`,
/// `-1_000 → "-1,000"`. For floats see [`intcomma_f64`].
///
/// ```
/// use rustango::humanize::intcomma;
/// assert_eq!(intcomma(4500), "4,500");
/// assert_eq!(intcomma(1_234_567), "1,234,567");
/// assert_eq!(intcomma(-1_000), "-1,000");
/// assert_eq!(intcomma(0), "0");
/// ```
#[must_use]
pub fn intcomma(n: i64) -> String {
    format_with_commas_i64(n)
}

/// `intcomma` variant for `f64` — comma-separates the integer
/// portion, preserves the fractional part untouched.
/// `1234567.89 → "1,234,567.89"`.
///
/// ```
/// use rustango::humanize::intcomma_f64;
/// assert_eq!(intcomma_f64(1234567.89), "1,234,567.89");
/// assert_eq!(intcomma_f64(0.5), "0.5");
/// assert_eq!(intcomma_f64(-1000.25), "-1,000.25");
/// ```
#[must_use]
pub fn intcomma_f64(f: f64) -> String {
    let s = format!("{f}");
    let (sign, body) = if let Some(rest) = s.strip_prefix('-') {
        ("-", rest)
    } else {
        ("", s.as_str())
    };
    let formatted = if let Some((int_part, frac_part)) = body.split_once('.') {
        let int_with_commas = comma_separate_digits(int_part);
        format!("{int_with_commas}.{frac_part}")
    } else {
        comma_separate_digits(body)
    };
    format!("{sign}{formatted}")
}

fn intcomma_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    if let Some(n) = value.as_i64() {
        return Ok(to_value(intcomma(n))?);
    }
    if let Some(n) = value.as_u64() {
        return Ok(to_value(format_with_commas_u64(n))?);
    }
    if let Some(f) = value.as_f64() {
        return Ok(to_value(intcomma_f64(f))?);
    }
    Ok(value.clone())
}

fn format_with_commas_i64(n: i64) -> String {
    let s = n.abs().to_string();
    let sep = comma_separate_digits(&s);
    if n < 0 {
        format!("-{sep}")
    } else {
        sep
    }
}

fn format_with_commas_u64(n: u64) -> String {
    comma_separate_digits(&n.to_string())
}

/// Insert commas into a digit-string at every 3-digit boundary from the right.
fn comma_separate_digits(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ------------------------------------------------------------------ format_number / format_currency
//
// Django-parity #426 + #428 — locale-aware number + currency
// formatting via two Tera filters. Hard-coded table for common
// locales; CLDR-driven dynamic locale data is a future-backlog
// item (the dep weight isn't justified for v1).
//
// Supported locales (decimal sep, thousands sep, currency template):
//
//   en / en-US: 1,234,567.89    USD $1,234.56 / EUR €1,234.56
//   en-GB:     1,234,567.89     GBP £1,234.56
//   de:        1.234.567,89     EUR 1.234,56 €
//   fr:        1 234 567,89     EUR 1 234,56 €
//   es:        1.234.567,89     EUR 1.234,56 €
//   it:        1.234.567,89     EUR 1.234,56 €
//   ja:        1,234,567.89     JPY ¥1,234 (no decimals for JPY)
//   zh:        1,234,567.89     CNY ¥1,234.56
//   pt:        1.234.567,89     EUR 1.234,56 €
//   ru:        1 234 567,89     RUB 1 234,56 ₽
//
// Unknown locales fall back to en-US convention with a
// `tracing::warn!` so misspelled locale codes surface in logs.

/// Per-locale numeric format spec — decimal point + group separator.
#[derive(Debug, Clone, Copy)]
struct NumberFmt {
    decimal: char,
    group: char,
}

fn locale_number_fmt(locale: &str) -> NumberFmt {
    // Normalize: lowercase, base-lang split. "en-US" → "en"; the
    // few region-specific entries (en-GB stays en for numbers,
    // pt-BR stays pt for numbers) match here.
    let base = locale.to_ascii_lowercase();
    let base = base.split('-').next().unwrap_or(&base);
    match base {
        // en family (incl. zh-CN where it's also dot+comma)
        "en" | "ja" | "zh" | "ko" | "th" => NumberFmt {
            decimal: '.',
            group: ',',
        },
        // de / es / it / nl / pt: dot grouping, comma decimal
        "de" | "es" | "it" | "nl" | "pt" | "el" | "pl" | "tr" | "da" | "fi" | "sv" | "no"
        | "nb" | "nn" => NumberFmt {
            decimal: ',',
            group: '.',
        },
        // fr / ru / cs / sk: thin/regular space grouping, comma decimal
        "fr" | "ru" | "cs" | "sk" | "bg" | "uk" | "hu" => NumberFmt {
            decimal: ',',
            group: ' ',
        },
        // Unknown — fall back to en-US with a warn.
        _ => {
            tracing::warn!(
                target: "rustango::humanize",
                locale = %locale,
                "format_number: unknown locale, falling back to en-US convention"
            );
            NumberFmt {
                decimal: '.',
                group: ',',
            }
        }
    }
}

/// Apply `fmt` to a digit string with optional fractional part.
/// `integer_part` is the digits before the decimal, `frac_part` is
/// the digits after (or `""`). Negative sign carried by caller.
fn apply_number_fmt(integer_part: &str, frac_part: &str, fmt: NumberFmt) -> String {
    let bytes = integer_part.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3 + frac_part.len() + 1);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(fmt.group);
        }
        out.push(*b as char);
    }
    if !frac_part.is_empty() {
        out.push(fmt.decimal);
        out.push_str(frac_part);
    }
    out
}

/// `format_number` — Django-parity #426. Locale-aware decimal +
/// thousands separator. Number-only; currency symbols handled by
/// [`format_currency`] below.
///
/// ```jinja
/// {{ 1234567.89 | format_number(locale="en") }}     {# → 1,234,567.89 #}
/// {{ 1234567.89 | format_number(locale="de") }}     {# → 1.234.567,89 #}
/// {{ 1234567.89 | format_number(locale="fr") }}     {# → 1 234 567,89 #}
/// {{ 1234.5    | format_number(locale="en", decimals=2) }}  {# → 1,234.50 #}
/// ```
///
/// Arguments:
/// - `locale` (string, default `"en"`) — locale code; only the
///   base language is consulted (`en-US` ≡ `en`). Unknown locales
///   fall back to `en` with a tracing warning.
/// - `decimals` (integer, optional) — fixed decimal places. When
///   set, the fractional part is padded/truncated to exactly this
///   many digits. When unset, the input's natural precision is
///   preserved.
///
/// Non-numeric input passes through unchanged.
/// Locale-aware number formatter — public Rust API.
///
/// Produces a string with the correct decimal point + thousands
/// separator for the given locale code. The base language is
/// consulted (`"en-US" ≡ "en"`); unknown locales fall back to
/// `"en"` with a `tracing::warn!`.
///
/// `decimals = None` preserves the input's natural precision
/// (`1234.5 → "1,234.5"`). `decimals = Some(n)` truncates or
/// pads the fractional part to exactly `n` digits
/// (`1234.0 + Some(2) → "1,234.00"`).
///
/// Supported locales (decimal sep, thousands sep):
/// * en / en-US / en-GB / ja / zh / ko / th: `.` decimal, `,` group
/// * de / es / it / nl / pt / pl / sv / nb / nn / da / fi / el /
///   tr: `,` decimal, `.` group
/// * fr / ru / cs / sk / bg / uk / hu: `,` decimal, space group
///
/// ```
/// use rustango::humanize::format_number;
/// assert_eq!(format_number(1234567.89, "en", None),    "1,234,567.89");
/// assert_eq!(format_number(1234567.89, "de", None),    "1.234.567,89");
/// assert_eq!(format_number(1234567.89, "fr", None),    "1 234 567,89");
/// assert_eq!(format_number(1234.5,     "en", Some(2)), "1,234.50");
/// assert_eq!(format_number(0.0,        "en", Some(0)), "0");
/// assert_eq!(format_number(-1234.5,    "en", None),    "-1,234.5");
/// ```
#[must_use]
pub fn format_number(value: f64, locale: &str, decimals: Option<usize>) -> String {
    let fmt = locale_number_fmt(locale);
    let s = match decimals {
        Some(d) => format!("{value:.*}", d),
        None => format!("{value}"),
    };
    let negative = s.starts_with('-');
    let body = if negative { &s[1..] } else { &s };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let formatted = apply_number_fmt(int_part, frac_part, fmt);
    if negative {
        format!("-{formatted}")
    } else {
        formatted
    }
}

fn format_number_filter(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let locale = args.get("locale").and_then(Value::as_str).unwrap_or("en");
    let decimals = args
        .get("decimals")
        .and_then(Value::as_u64)
        .map(|n| n as usize);

    // Distinct integer-path branch: integers should NOT acquire a
    // trailing decimal-and-zeros unless `decimals` was explicitly
    // set. Filter wrapper preserves this by short-circuiting
    // through `apply_number_fmt` directly with `frac = "0".repeat(d)`
    // for the int case.
    if let Some(n) = value.as_i64() {
        let fmt = locale_number_fmt(locale);
        let abs = n.unsigned_abs().to_string();
        let frac = match decimals {
            Some(d) => "0".repeat(d),
            None => String::new(),
        };
        let body = apply_number_fmt(&abs, &frac, fmt);
        return Ok(to_value(if n < 0 { format!("-{body}") } else { body })?);
    }
    if let Some(n) = value.as_u64() {
        let fmt = locale_number_fmt(locale);
        let frac = match decimals {
            Some(d) => "0".repeat(d),
            None => String::new(),
        };
        return Ok(to_value(apply_number_fmt(&n.to_string(), &frac, fmt))?);
    }
    if let Some(f) = value.as_f64() {
        return Ok(to_value(format_number(f, locale, decimals))?);
    }
    Ok(value.clone())
}

/// Per-locale currency-display spec.
#[derive(Debug, Clone, Copy)]
struct CurrencyFmt {
    /// Currency symbol (e.g. "$", "€", "£", "¥", "₽").
    symbol: &'static str,
    /// `true` if the symbol prefixes the amount (`$1,234.56`),
    /// `false` if it suffixes (`1.234,56 €`).
    prefix: bool,
    /// Decimal places. Most currencies use 2; JPY/KRW/CLP use 0.
    decimals: u32,
}

fn currency_fmt(code: &str) -> CurrencyFmt {
    let code = code.to_ascii_uppercase();
    match code.as_str() {
        "USD" | "CAD" | "AUD" | "NZD" | "HKD" | "SGD" | "MXN" => CurrencyFmt {
            symbol: "$",
            prefix: true,
            decimals: 2,
        },
        "EUR" => CurrencyFmt {
            symbol: "€",
            prefix: true,
            decimals: 2,
        },
        "GBP" => CurrencyFmt {
            symbol: "£",
            prefix: true,
            decimals: 2,
        },
        "JPY" | "KRW" | "CLP" => CurrencyFmt {
            // No fractional sub-unit in circulation.
            symbol: "¥",
            prefix: true,
            decimals: 0,
        },
        "CNY" => CurrencyFmt {
            symbol: "¥",
            prefix: true,
            decimals: 2,
        },
        "RUB" => CurrencyFmt {
            symbol: "₽",
            prefix: false,
            decimals: 2,
        },
        "INR" => CurrencyFmt {
            symbol: "₹",
            prefix: true,
            decimals: 2,
        },
        "BRL" => CurrencyFmt {
            symbol: "R$",
            prefix: true,
            decimals: 2,
        },
        "CHF" => CurrencyFmt {
            symbol: "CHF",
            prefix: true,
            decimals: 2,
        },
        // Unknown — emit the code itself as the symbol so the
        // output is still meaningful.
        _ => {
            tracing::warn!(
                target: "rustango::humanize",
                currency = %code,
                "format_currency: unknown ISO 4217 code, using raw code as symbol"
            );
            CurrencyFmt {
                symbol: Box::leak(code.into_boxed_str()),
                prefix: true,
                decimals: 2,
            }
        }
    }
}

/// `format_currency` — Django-parity #428. Locale-aware currency
/// rendering. Composes [`format_number`] with a per-currency
/// symbol + placement convention.
///
/// ```jinja
/// {{ 1234.5 | format_currency(currency="USD") }}              {# → $1,234.50 #}
/// {{ 1234.5 | format_currency(currency="EUR", locale="de") }} {# → €1.234,50 (de places symbol after via locale-specific override; here prefix is default) #}
/// {{ 1234.5 | format_currency(currency="EUR", locale="fr") }} {# → 1 234,50 € #}
/// {{ 1234   | format_currency(currency="JPY") }}              {# → ¥1,234 (0 decimals) #}
/// ```
///
/// Arguments:
/// - `currency` (string, default `"USD"`) — ISO 4217 currency
///   code. Unknown codes use the code itself as the "symbol"
///   prefix and log a warning.
/// - `locale` (string, default `"en"`) — locale code for the
///   thousands/decimal separators. Decimal places come from the
///   currency, not the locale.
///
/// Symbol placement convention:
/// - For currencies with their own placement (RUB suffix, USD
///   prefix), the currency wins.
/// - For Euro: prefix in en/de, suffix in fr/it/es (matches local
///   typography conventions).
///
/// Non-numeric input passes through unchanged.
fn format_currency(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let currency = args
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or("USD");
    let locale = args.get("locale").and_then(Value::as_str).unwrap_or("en");

    let cur = currency_fmt(currency);
    let fmt = locale_number_fmt(locale);

    let amount = match value.as_f64() {
        Some(f) => f,
        None => match value.as_i64() {
            Some(i) => i as f64,
            None => match value.as_u64() {
                Some(u) => u as f64,
                None => return Ok(value.clone()),
            },
        },
    };

    let body_str = format!("{amount:.*}", cur.decimals as usize);
    let negative = body_str.starts_with('-');
    let unsigned = if negative { &body_str[1..] } else { &body_str };
    let (int_part, frac_part) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let formatted = apply_number_fmt(int_part, frac_part, fmt);
    let sign = if negative { "-" } else { "" };

    // Locale override for Euro placement — French / Italian /
    // Spanish / Portuguese put the symbol after with a space.
    let base = locale.to_ascii_lowercase();
    let base = base.split('-').next().unwrap_or(&base);
    let euro_suffix_locale = matches!(base, "fr" | "it" | "es" | "pt" | "nl");
    let prefix_after_override = if currency.eq_ignore_ascii_case("EUR") && euro_suffix_locale {
        false
    } else {
        cur.prefix
    };

    let out = if prefix_after_override {
        format!("{sign}{symbol}{formatted}", symbol = cur.symbol)
    } else {
        format!("{sign}{formatted} {symbol}", symbol = cur.symbol)
    };

    Ok(to_value(out)?)
}

// ------------------------------------------------------------------ intword

/// `intword` — large numbers as words. Django:
/// `1_200_000 → "1.2 million"`, `1_000_000_000 → "1.0 billion"`.
/// Below 1 million the number passes through as-is.
/// [`django.contrib.humanize.intword`](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/#intword) —
/// large numbers as words. `1_200_000 → "1.2 million"`,
/// `1_000_000_000 → "1.0 billion"`. Below 1 million the integer
/// passes through unformatted ("123" not "1.2e2" or "123.0").
///
/// Scales recognized: million / billion / trillion / quadrillion
/// / quintillion / sextillion / septillion / octillion /
/// nonillion / decillion (`1e6 .. 1e33`). Values beyond decillion
/// stay on the decillion scale (Django shape).
///
/// ```
/// use rustango::humanize::intword;
/// assert_eq!(intword(1_200_000.0), "1.2 million");
/// assert_eq!(intword(1_000_000_000.0), "1.0 billion");
/// assert_eq!(intword(999_999.0), "999999");
/// assert_eq!(intword(-1_500_000.0), "-1.5 million");
/// ```
pub fn intword(n: f64) -> String {
    if n.abs() < 1_000_000.0 {
        // Django returns the integer unformatted for < 1M.
        return format!("{}", n.trunc() as i64);
    }
    let scales: &[(f64, &str)] = &[
        (1e6, "million"),
        (1e9, "billion"),
        (1e12, "trillion"),
        (1e15, "quadrillion"),
        (1e18, "quintillion"),
        (1e21, "sextillion"),
        (1e24, "septillion"),
        (1e27, "octillion"),
        (1e30, "nonillion"),
        (1e33, "decillion"),
    ];
    let mut chosen = scales[0];
    for &(s, name) in scales {
        if n.abs() >= s {
            chosen = (s, name);
        } else {
            break;
        }
    }
    let scaled = n / chosen.0;
    format!("{:.1} {}", scaled, chosen.1)
}

fn intword_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v as f64,
        None => match value.as_f64() {
            Some(v) => v,
            None => return Ok(value.clone()),
        },
    };
    if n.abs() < 1_000_000.0 {
        if let Some(i) = value.as_i64() {
            return Ok(to_value(i.to_string())?);
        }
        return Ok(value.clone());
    }
    Ok(to_value(intword(n))?)
}

// ------------------------------------------------------------------ naturalsize

/// `naturalsize` — bytes formatted human-readable (binary KiB-scale).
/// `1024 → "1.0 KB"`, `1536 → "1.5 KB"`, `1_572_864 → "1.5 MB"`.
/// Falls back to bytes for values < 1024.
/// [`django.contrib.humanize.naturalsize`](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/#naturalsize) —
/// bytes formatted human-readable (binary KiB-scale: 1024).
/// `1024 → "1.0 KB"`, `1_572_864 → "1.5 MB"`. Values below 1024
/// return `"N bytes"` (or `"1 byte"` for `n == 1`).
///
/// Units recognized: bytes, KB, MB, GB, TB, PB, EB, ZB, YB.
///
/// ```
/// use rustango::humanize::naturalsize;
/// assert_eq!(naturalsize(1024.0), "1.0 KB");
/// assert_eq!(naturalsize(1_572_864.0), "1.5 MB");
/// assert_eq!(naturalsize(1.0), "1 byte");
/// assert_eq!(naturalsize(0.0), "0 bytes");
/// ```
pub fn naturalsize(n: f64) -> String {
    let units = ["bytes", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    if n < 1024.0 {
        if (n - 1.0).abs() < f64::EPSILON {
            return "1 byte".to_owned();
        }
        return format!("{} bytes", n as u64);
    }
    let mut scale = 0_usize;
    let mut scaled = n;
    while scaled >= 1024.0 && scale < units.len() - 1 {
        scaled /= 1024.0;
        scale += 1;
    }
    format!("{:.1} {}", scaled, units[scale])
}

fn naturalsize_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_u64() {
        Some(v) => v as f64,
        None => match value.as_i64() {
            Some(v) if v >= 0 => v as f64,
            _ => match value.as_f64() {
                Some(v) => v,
                None => return Ok(value.clone()),
            },
        },
    };
    Ok(to_value(naturalsize(n))?)
}

// ------------------------------------------------------------------ ordinal

/// `ordinal` — append the appropriate English ordinal suffix.
/// `1 → "1st"`, `2 → "2nd"`, `3 → "3rd"`, `4 → "4th"`, `11 → "11th"`,
/// `21 → "21st"`. Negative numbers get the same suffix as their
/// absolute value.
/// [`django.contrib.humanize.ordinal`](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/#ordinal) —
/// append the appropriate English ordinal suffix.
/// `1 → "1st"`, `2 → "2nd"`, `3 → "3rd"`, `4 → "4th"`,
/// `11 → "11th"`, `21 → "21st"`. Teens (11/12/13) always take
/// "th"; everything else falls through to the last-digit rule.
/// Negative numbers get the same suffix as their absolute value.
///
/// ```
/// use rustango::humanize::ordinal;
/// assert_eq!(ordinal(1), "1st");
/// assert_eq!(ordinal(2), "2nd");
/// assert_eq!(ordinal(11), "11th");
/// assert_eq!(ordinal(21), "21st");
/// assert_eq!(ordinal(-3), "-3rd");
/// ```
#[must_use]
pub fn ordinal(n: i64) -> String {
    format!("{n}{}", ordinal_suffix(n.unsigned_abs()))
}

fn ordinal_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v,
        None => return Ok(value.clone()),
    };
    Ok(to_value(ordinal(n))?)
}

fn ordinal_suffix(n: u64) -> &'static str {
    // 11/12/13 are "th" — special-case the teens before falling
    // through to the last-digit branch.
    let last_two = n % 100;
    if (11..=13).contains(&last_two) {
        return "th";
    }
    match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

// ------------------------------------------------------------------ apnumber

/// [`django.contrib.humanize.apnumber`](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/#apnumber) —
/// spell out small numbers (1..=9 → `"one"`..`"nine"`); other
/// values stringify as the integer. Matches the AP style guide
/// that Django adopts.
///
/// ```
/// use rustango::humanize::apnumber;
/// assert_eq!(apnumber(1), "one");
/// assert_eq!(apnumber(9), "nine");
/// assert_eq!(apnumber(10), "10");
/// assert_eq!(apnumber(0), "0");
/// assert_eq!(apnumber(-3), "-3");
/// ```
#[must_use]
pub fn apnumber(n: i64) -> String {
    match n {
        1 => "one".to_owned(),
        2 => "two".to_owned(),
        3 => "three".to_owned(),
        4 => "four".to_owned(),
        5 => "five".to_owned(),
        6 => "six".to_owned(),
        7 => "seven".to_owned(),
        8 => "eight".to_owned(),
        9 => "nine".to_owned(),
        _ => n.to_string(),
    }
}

fn apnumber_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v,
        None => return Ok(value.clone()),
    };
    Ok(to_value(apnumber(n))?)
}

// ------------------------------------------------------------------ naturaltime

/// `naturaltime` — relative time string compared to "now".
/// `"3 minutes ago"`, `"in 5 hours"`, `"just now"`. Accepts RFC3339
/// strings or anything serde-parsable as `DateTime<Utc>`.
///
/// Bucket thresholds match Django's `naturaltime`:
/// - <30s → "now"
/// - <60s → "N seconds {ago,from now}"
/// - <60m → "N minutes {ago,from now}"
/// - <24h → "N hours {ago,from now}"
/// - <30d → "N days {ago,from now}"
/// - <365d → "N months {ago,from now}"
/// - ≥365d → "N years {ago,from now}"
fn naturaltime_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    Ok(to_value(natural_time_string(now, dt))?)
}

fn parse_datetime(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = value.as_str() {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    serde_json::from_value(value.clone()).ok()
}

/// [`django.contrib.humanize.naturaltime`](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/#naturaltime) —
/// relative time string compared to `now`. `"3 minutes ago"`,
/// `"in 5 hours"`, `"now"` (within 30 s either direction).
///
/// Bucket thresholds match Django:
/// `<30s` → "now"; `<60s` → seconds; `<60m` → minutes;
/// `<24h` → hours; `<30d` → days; `<12mo` → months; else years.
///
/// Bucketing is single-unit (top bucket only). For depth-respecting
/// "4 days, 6 hours" output use [`crate::timesince::timesince`].
///
/// ```
/// use chrono::{Duration, TimeZone, Utc};
/// use rustango::humanize::naturaltime;
///
/// let now = Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap();
/// let past = now - Duration::minutes(3);
/// assert_eq!(naturaltime(now, past), "3 minutes ago");
///
/// let future = now + Duration::hours(5);
/// assert_eq!(naturaltime(now, future), "in 5 hours");
///
/// assert_eq!(naturaltime(now, now), "now");
/// ```
#[must_use]
pub fn naturaltime(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    natural_time_string(now, then)
}

/// [`django.contrib.humanize.naturalday`](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/#naturalday) —
/// calendar-relative day name. `"today"`, `"yesterday"`,
/// `"tomorrow"`, else `"Mmm DD"` (e.g. `"Apr 27"`).
///
/// The fallback `"Mmm DD"` matches Django's default `DATE_FORMAT`
/// when no operator override is in force.
///
/// ```
/// use chrono::{Duration, TimeZone, Utc};
/// use rustango::humanize::naturalday;
///
/// let now = Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap();
/// assert_eq!(naturalday(now, now), "today");
/// assert_eq!(naturalday(now, now - Duration::days(1)), "yesterday");
/// assert_eq!(naturalday(now, now + Duration::days(1)), "tomorrow");
/// assert_eq!(naturalday(now, now - Duration::days(45)),
///            "Apr 21");
/// ```
#[must_use]
pub fn naturalday(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    natural_day_string(now, then)
}

fn natural_time_string(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let abs = delta.num_seconds().abs();
    let suffix = if delta.num_seconds() >= 0 {
        "ago"
    } else {
        "from now"
    };

    if abs < 30 {
        return "now".to_owned();
    }
    if abs < 60 {
        return format_unit(abs, "second", suffix);
    }
    let minutes = abs / 60;
    if minutes < 60 {
        return format_unit(minutes, "minute", suffix);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format_unit(hours, "hour", suffix);
    }
    let days = hours / 24;
    if days < 30 {
        return format_unit(days, "day", suffix);
    }
    let months = days / 30;
    if months < 12 {
        return format_unit(months, "month", suffix);
    }
    let years = days / 365;
    format_unit(years, "year", suffix)
}

fn format_unit(n: i64, unit: &str, suffix: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    if suffix == "ago" {
        format!("{n} {unit}{plural} ago")
    } else {
        format!("in {n} {unit}{plural}")
    }
}

// ------------------------------------------------------------------ naturalday

fn naturalday_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    Ok(to_value(natural_day_string(Utc::now(), dt))?)
}

fn natural_day_string(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let today = now.date_naive();
    let other = then.date_naive();
    let diff = (other - today).num_days();
    match diff {
        0 => "today".to_owned(),
        -1 => "yesterday".to_owned(),
        1 => "tomorrow".to_owned(),
        _ => {
            let month = match other.month() {
                1 => "Jan",
                2 => "Feb",
                3 => "Mar",
                4 => "Apr",
                5 => "May",
                6 => "Jun",
                7 => "Jul",
                8 => "Aug",
                9 => "Sep",
                10 => "Oct",
                11 => "Nov",
                12 => "Dec",
                _ => unreachable!(),
            };
            format!("{month} {:02}", other.day())
        }
    }
}

// ------------------------------------------------------------------ timesince / timeuntil

/// Magnitude-only equivalent of [`natural_time_string`]: emits
/// `"N units"` without an `"ago"` / `"in"` decorator. Used by
/// [`timesince`] / [`timeuntil`].
///
/// Returns `"0 minutes"` for non-positive deltas — Django's
/// `timesince` does the same (negative deltas indicate the page
/// rendered AFTER the target, which we treat as "no time has
/// passed yet").
fn magnitude_string(seconds: i64) -> String {
    if seconds <= 0 {
        return "0 minutes".to_owned();
    }
    if seconds < 60 {
        return format_magnitude(seconds, "second");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format_magnitude(minutes, "minute");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format_magnitude(hours, "hour");
    }
    let days = hours / 24;
    if days < 30 {
        return format_magnitude(days, "day");
    }
    let months = days / 30;
    if months < 12 {
        return format_magnitude(months, "month");
    }
    let years = days / 365;
    format_magnitude(years, "year")
}

fn format_magnitude(n: i64, unit: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural}")
}

/// `timesince` — duration from `value` to now, formatted as
/// `"N units"`. Django's `{{ post.created | timesince }}` shape.
/// Returns `"0 minutes"` when the input is in the future (caller
/// likely wants [`timeuntil`] for that case).
///
/// Bucketing matches [`naturaltime`] — seconds / minutes / hours
/// / days / months (30-day) / years (365-day) — and pluralization
/// drops the trailing `s` only for `1`.
fn timesince(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    let delta = now.signed_duration_since(dt).num_seconds();
    Ok(to_value(magnitude_string(delta))?)
}

/// `timeuntil` — duration from now to `value`, formatted as
/// `"N units"`. Mirror of [`timesince`] for future-pointing values:
/// `{{ event.start | timeuntil }}` → `"3 days"`. Past timestamps
/// produce `"0 minutes"`.
fn timeuntil(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    let delta = dt.signed_duration_since(now).num_seconds();
    Ok(to_value(magnitude_string(delta))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn render(tera: &Tera, src: &str, ctx: tera::Context) -> String {
        let mut t = tera.clone();
        t.add_raw_template("_", src).unwrap();
        t.render("_", &ctx).unwrap()
    }

    fn setup() -> Tera {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera
    }

    // -------- intcomma --------

    #[test]
    fn intcomma_handles_small_ints() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &450_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "450");
    }

    #[test]
    fn intcomma_inserts_separators() {
        let tera = setup();
        for (n, expected) in [
            (1_234_i64, "1,234"),
            (1_234_567, "1,234,567"),
            (1_000_000_000, "1,000,000,000"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(
                render(&tera, "{{ n | intcomma }}", ctx),
                expected,
                "for n={n}"
            );
        }
    }

    #[test]
    fn intcomma_handles_negative_ints() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &-1_234_567_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "-1,234,567");
    }

    #[test]
    fn intcomma_preserves_decimal_part() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &1_234_567.89_f64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "1,234,567.89");
    }

    // -------- intword --------

    #[test]
    fn intword_below_million_unchanged() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &999_999_i64);
        assert_eq!(render(&tera, "{{ n | intword }}", ctx), "999999");
    }

    #[test]
    fn intword_million_scale() {
        let tera = setup();
        for (n, expected) in [
            (1_200_000_i64, "1.2 million"),
            (1_000_000, "1.0 million"),
            (2_500_000_000, "2.5 billion"),
            (1_000_000_000_000_i64, "1.0 trillion"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | intword }}", ctx), expected, "n={n}");
        }
    }

    // -------- naturalsize --------

    #[test]
    fn naturalsize_byte_threshold() {
        let tera = setup();
        for (bytes, expected) in [
            (0_u64, "0 bytes"),
            (1, "1 byte"),
            (512, "512 bytes"),
            (1_023, "1023 bytes"),
            (1_024, "1.0 KB"),
            (1_536, "1.5 KB"),
            (1_572_864, "1.5 MB"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("b", &bytes);
            assert_eq!(
                render(&tera, "{{ b | naturalsize }}", ctx),
                expected,
                "bytes={bytes}"
            );
        }
    }

    // -------- ordinal --------

    #[test]
    fn ordinal_picks_correct_suffix() {
        let tera = setup();
        for (n, expected) in [
            (1_i64, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (10, "10th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (14, "14th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (101, "101st"),
            (111, "111th"),
            (112, "112th"),
            (113, "113th"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | ordinal }}", ctx), expected, "n={n}");
        }
    }

    // -------- apnumber --------

    #[test]
    fn apnumber_spells_one_through_nine() {
        let tera = setup();
        for (n, expected) in [
            (1_i64, "one"),
            (5, "five"),
            (9, "nine"),
            (10, "10"),
            (42, "42"),
            (0, "0"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | apnumber }}", ctx), expected, "n={n}");
        }
    }

    // -------- naturaltime --------

    #[test]
    fn naturaltime_buckets_correctly() {
        let now = Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap();
        for (offset_secs, expected) in [
            (5, "now"),
            (45, "45 seconds ago"),
            (-45, "in 45 seconds"),
            (60, "1 minute ago"),
            (120, "2 minutes ago"),
            (3600, "1 hour ago"),
            (7200, "2 hours ago"),
            (86_400, "1 day ago"),
            (86_400 * 2, "2 days ago"),
            (86_400 * 31, "1 month ago"),
            (86_400 * 400, "1 year ago"),
            (-3600, "in 1 hour"),
        ] {
            let then = now - Duration::seconds(offset_secs);
            assert_eq!(
                natural_time_string(now, then),
                expected,
                "offset={offset_secs}"
            );
        }
    }

    // -------- naturalday --------

    #[test]
    fn naturalday_keywords() {
        let now = Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap();
        let today = now;
        let yesterday = now - Duration::days(1);
        let tomorrow = now + Duration::days(1);
        let week_ago = now - Duration::days(7);
        assert_eq!(natural_day_string(now, today), "today");
        assert_eq!(natural_day_string(now, yesterday), "yesterday");
        assert_eq!(natural_day_string(now, tomorrow), "tomorrow");
        // 7 days back from 2026-05-16 = 2026-05-09
        assert_eq!(natural_day_string(now, week_ago), "May 09");
    }

    // -------- register --------

    #[test]
    fn register_filters_makes_them_callable_via_tera() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &1_000_000_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "1,000,000");
    }

    // -------- timesince / timeuntil --------

    #[test]
    fn magnitude_string_buckets_match_naturaltime() {
        assert_eq!(magnitude_string(0), "0 minutes");
        assert_eq!(magnitude_string(-5), "0 minutes");
        assert_eq!(magnitude_string(1), "1 second");
        assert_eq!(magnitude_string(45), "45 seconds");
        assert_eq!(magnitude_string(60), "1 minute");
        assert_eq!(magnitude_string(120), "2 minutes");
        assert_eq!(magnitude_string(60 * 60), "1 hour");
        assert_eq!(magnitude_string(60 * 60 * 5), "5 hours");
        assert_eq!(magnitude_string(60 * 60 * 24), "1 day");
        assert_eq!(magnitude_string(60 * 60 * 24 * 31), "1 month");
        assert_eq!(magnitude_string(60 * 60 * 24 * 366), "1 year");
    }

    #[test]
    fn timesince_filter_emits_magnitude_for_past() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        // 2 hours ago, give-or-take.
        let then = Utc::now() - Duration::hours(2);
        ctx.insert("then", &then.to_rfc3339());
        let out = render(&tera, "{{ then | timesince }}", ctx);
        assert_eq!(out, "2 hours");
    }

    #[test]
    fn timesince_filter_emits_zero_for_future() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        let later = Utc::now() + Duration::hours(2);
        ctx.insert("later", &later.to_rfc3339());
        let out = render(&tera, "{{ later | timesince }}", ctx);
        assert_eq!(out, "0 minutes");
    }

    #[test]
    fn timeuntil_filter_emits_magnitude_for_future() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        // Use a wider gap so test-wall-clock-drift between insert
        // and render-time doesn't bump us across the day boundary.
        let later = Utc::now() + Duration::days(3) + Duration::hours(1);
        ctx.insert("later", &later.to_rfc3339());
        let out = render(&tera, "{{ later | timeuntil }}", ctx);
        assert_eq!(out, "3 days", "got: {out}");
    }

    #[test]
    fn timeuntil_filter_emits_zero_for_past() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        let then = Utc::now() - Duration::days(3);
        ctx.insert("then", &then.to_rfc3339());
        let out = render(&tera, "{{ then | timeuntil }}", ctx);
        assert_eq!(out, "0 minutes");
    }

    #[test]
    fn timesince_pluralizes_correctly() {
        // 1 second → "1 second"; 2 → "2 seconds"
        assert_eq!(magnitude_string(1), "1 second");
        assert_eq!(magnitude_string(2), "2 seconds");
        // 1 minute → "1 minute"; 2 → "2 minutes"
        assert_eq!(magnitude_string(60), "1 minute");
        assert_eq!(magnitude_string(120), "2 minutes");
    }

    // -------- #426 / #428 — format_number + format_currency --------

    fn render_filter(template: &str, ctx: tera::Context) -> String {
        let mut tera = Tera::default();
        tera.add_raw_template("t", template).unwrap();
        register_filters(&mut tera);
        tera.render("t", &ctx).unwrap()
    }

    #[test]
    fn format_number_en_uses_comma_grouping_dot_decimal() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234567.89_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="en") }}"#, ctx),
            "1,234,567.89"
        );
    }

    #[test]
    fn format_number_de_uses_dot_grouping_comma_decimal() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234567.89_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="de") }}"#, ctx),
            "1.234.567,89"
        );
    }

    #[test]
    fn format_number_fr_uses_space_grouping_comma_decimal() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234567.89_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="fr") }}"#, ctx),
            "1 234 567,89"
        );
    }

    #[test]
    fn format_number_decimals_arg_pads_and_truncates() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        let out = render_filter(r#"{{ x | format_number(locale="en", decimals=2) }}"#, ctx);
        assert_eq!(out, "1,234.50");

        let mut ctx2 = tera::Context::new();
        ctx2.insert("x", &1234.5678_f64);
        let out2 = render_filter(r#"{{ x | format_number(locale="en", decimals=2) }}"#, ctx2);
        assert_eq!(out2, "1,234.57");
    }

    #[test]
    fn format_number_negative_carries_sign() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &-1234.5_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="en") }}"#, ctx),
            "-1,234.5"
        );
    }

    #[test]
    fn format_number_integer_input_works() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1_234_567i64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="en") }}"#, ctx),
            "1,234,567"
        );
    }

    #[test]
    fn format_number_unknown_locale_falls_back_to_en() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        // "xx-YZ" has no entry; falls back to en-US.
        let out = render_filter(r#"{{ x | format_number(locale="xx-YZ") }}"#, ctx);
        assert_eq!(out, "1,234.5");
    }

    #[test]
    fn format_currency_usd_en_renders_dollar_prefix_2dp() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_currency(currency="USD") }}"#, ctx),
            "$1,234.50"
        );
    }

    #[test]
    fn format_currency_eur_fr_renders_symbol_suffix() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        let out = render_filter(
            r#"{{ x | format_currency(currency="EUR", locale="fr") }}"#,
            ctx,
        );
        assert_eq!(out, "1 234,50 €");
    }

    #[test]
    fn format_currency_jpy_uses_zero_decimals() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.567_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_currency(currency="JPY") }}"#, ctx),
            "¥1,235"
        );
    }

    #[test]
    fn format_currency_negative_amount() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &-1234.5_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_currency(currency="USD") }}"#, ctx),
            "-$1,234.50"
        );
    }

    #[test]
    fn format_currency_unknown_code_uses_code_as_symbol() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.0_f64);
        // ZZX is not a real ISO 4217 code.
        let out = render_filter(r#"{{ x | format_currency(currency="ZZX") }}"#, ctx);
        assert!(out.contains("ZZX"), "got: {out}");
        assert!(out.contains("1,234.00"), "got: {out}");
    }

    // -------- Public Rust API (extracted from Tera filters) --------

    #[test]
    fn intword_public_basic() {
        assert_eq!(intword(1_200_000.0), "1.2 million");
        assert_eq!(intword(1_000_000_000.0), "1.0 billion");
        assert_eq!(intword(2_500_000_000_000.0), "2.5 trillion");
    }

    #[test]
    fn intword_public_below_million_unformatted() {
        assert_eq!(intword(999_999.0), "999999");
        assert_eq!(intword(0.0), "0");
        assert_eq!(intword(42.0), "42");
    }

    #[test]
    fn intword_public_negative() {
        assert_eq!(intword(-1_500_000.0), "-1.5 million");
    }

    #[test]
    fn naturalsize_public_basic() {
        assert_eq!(naturalsize(0.0), "0 bytes");
        assert_eq!(naturalsize(1.0), "1 byte");
        assert_eq!(naturalsize(512.0), "512 bytes");
        assert_eq!(naturalsize(1024.0), "1.0 KB");
        assert_eq!(naturalsize(1_572_864.0), "1.5 MB");
    }

    #[test]
    fn naturalsize_public_top_scale_caps() {
        // 2^80 bytes — fits exactly in YB scale (the last entry).
        let n = 1024.0_f64.powi(8);
        let out = naturalsize(n);
        assert!(out.ends_with("YB"), "got: {out}");
    }

    #[test]
    fn ordinal_public_basic() {
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(4), "4th");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(12), "12th");
        assert_eq!(ordinal(13), "13th");
        assert_eq!(ordinal(21), "21st");
        assert_eq!(ordinal(102), "102nd");
        assert_eq!(ordinal(113), "113th");
    }

    #[test]
    fn ordinal_public_negative_uses_abs_suffix() {
        assert_eq!(ordinal(-1), "-1st");
        assert_eq!(ordinal(-11), "-11th");
        assert_eq!(ordinal(-23), "-23rd");
    }

    #[test]
    fn apnumber_public_basic() {
        assert_eq!(apnumber(1), "one");
        assert_eq!(apnumber(5), "five");
        assert_eq!(apnumber(9), "nine");
        assert_eq!(apnumber(0), "0");
        assert_eq!(apnumber(10), "10");
        assert_eq!(apnumber(-3), "-3");
        assert_eq!(apnumber(100), "100");
    }

    // -------- Public naturaltime / naturalday --------

    fn ntime_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap()
    }

    #[test]
    fn naturaltime_now_within_30s() {
        let now = ntime_now();
        assert_eq!(naturaltime(now, now), "now");
        assert_eq!(naturaltime(now, now + Duration::seconds(20)), "now");
        assert_eq!(naturaltime(now, now - Duration::seconds(29)), "now");
    }

    #[test]
    fn naturaltime_seconds_ago_and_from_now() {
        let now = ntime_now();
        assert_eq!(
            naturaltime(now, now - Duration::seconds(45)),
            "45 seconds ago"
        );
        assert_eq!(
            naturaltime(now, now + Duration::seconds(45)),
            "in 45 seconds"
        );
    }

    #[test]
    fn naturaltime_singular_pluralization() {
        let now = ntime_now();
        assert_eq!(naturaltime(now, now - Duration::minutes(1)), "1 minute ago");
        assert_eq!(naturaltime(now, now + Duration::hours(1)), "in 1 hour");
    }

    #[test]
    fn naturaltime_bucket_transitions() {
        let now = ntime_now();
        assert_eq!(
            naturaltime(now, now - Duration::minutes(3)),
            "3 minutes ago"
        );
        assert_eq!(naturaltime(now, now - Duration::hours(5)), "5 hours ago");
        assert_eq!(naturaltime(now, now - Duration::days(10)), "10 days ago");
        // 35 days → 1 month
        assert_eq!(naturaltime(now, now - Duration::days(35)), "1 month ago");
        // 400 days → 1 year
        assert_eq!(naturaltime(now, now - Duration::days(400)), "1 year ago");
    }

    #[test]
    fn naturalday_today_yesterday_tomorrow() {
        let now = ntime_now();
        assert_eq!(naturalday(now, now), "today");
        assert_eq!(naturalday(now, now - Duration::days(1)), "yesterday");
        assert_eq!(naturalday(now, now + Duration::days(1)), "tomorrow");
    }

    #[test]
    fn naturalday_fallback_format() {
        let now = ntime_now();
        let other = now - Duration::days(45);
        let out = naturalday(now, other);
        // 2026-06-05 minus 45 days = 2026-04-21
        assert_eq!(out, "Apr 21");
    }

    // -------- Public intcomma / intcomma_f64 --------

    #[test]
    fn intcomma_public_basic() {
        assert_eq!(intcomma(0), "0");
        assert_eq!(intcomma(999), "999");
        assert_eq!(intcomma(4500), "4,500");
        assert_eq!(intcomma(1_234_567), "1,234,567");
    }

    #[test]
    fn intcomma_public_negative() {
        assert_eq!(intcomma(-1_000), "-1,000");
        assert_eq!(intcomma(-1_234_567), "-1,234,567");
    }

    #[test]
    fn intcomma_f64_public_basic() {
        assert_eq!(intcomma_f64(1234567.89), "1,234,567.89");
        assert_eq!(intcomma_f64(0.5), "0.5");
        assert_eq!(intcomma_f64(1000.0), "1,000");
    }

    #[test]
    fn intcomma_f64_public_negative() {
        assert_eq!(intcomma_f64(-1000.25), "-1,000.25");
    }

    // -------- Public format_number --------

    #[test]
    fn format_number_en_locale() {
        assert_eq!(format_number(1234567.89, "en", None), "1,234,567.89");
        assert_eq!(format_number(1234567.89, "en-US", None), "1,234,567.89");
        assert_eq!(format_number(1234567.89, "en-GB", None), "1,234,567.89");
    }

    #[test]
    fn format_number_de_locale() {
        assert_eq!(format_number(1234567.89, "de", None), "1.234.567,89");
    }

    #[test]
    fn format_number_fr_locale_space_thousands() {
        assert_eq!(format_number(1234567.89, "fr", None), "1 234 567,89");
    }

    #[test]
    fn format_number_with_decimals() {
        // decimals = Some pads / truncates to exact precision.
        assert_eq!(format_number(1234.5, "en", Some(2)), "1,234.50");
        assert_eq!(format_number(0.0, "en", Some(0)), "0");
        assert_eq!(format_number(1234.56789, "en", Some(2)), "1,234.57"); // rounds
    }

    #[test]
    fn format_number_negative() {
        assert_eq!(format_number(-1234.5, "en", None), "-1,234.5");
        assert_eq!(format_number(-1234.5, "de", Some(2)), "-1.234,50");
    }

    #[test]
    fn format_number_public_unknown_locale_falls_back_to_en() {
        // Returns the en-US shape on unknown.
        assert_eq!(format_number(1234.5, "xx-YY", None), "1,234.5");
    }
}
